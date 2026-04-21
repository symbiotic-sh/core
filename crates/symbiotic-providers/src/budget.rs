//! Credit budget enforcement for AI provider spending.
//!
//! [`BudgetEnforcer`] reads the usage log and checks against configured budget
//! limits before each request. The router calls [`BudgetEnforcer::check`] before
//! dispatching a request; if the budget is exceeded, it returns
//! [`ProviderError::BudgetExceeded`] with a descriptive message.
//!
//! Local providers ([`ProviderClass::Local`]) are never budget-limited, but that
//! decision is made by the caller (router), not here.

use std::sync::Arc;

use chrono::{Datelike, TimeZone, Utc};

use crate::config::{BudgetConfig, ProviderBudget};
use crate::error::ProviderError;
use crate::metering::{UsageFilter, UsageLog};
use crate::types::RequestType;

/// Checks spending against configured budgets before each request.
pub struct BudgetEnforcer {
    config: BudgetConfig,
    log: Arc<UsageLog>,
}

/// Start-of-day and start-of-month Unix timestamps.
struct TimeWindow {
    start_of_day: u64,
    start_of_month: u64,
}

/// Compute the start-of-day and start-of-month Unix timestamps for the current
/// UTC time derived from `now_unix`.
fn compute_time_window(now: u64) -> TimeWindow {
    let dt = Utc
        .timestamp_opt(now as i64, 0)
        .single()
        .unwrap_or_else(Utc::now);

    let start_of_day = dt
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .expect("midnight is always valid")
        .and_utc()
        .timestamp() as u64;

    let start_of_month = dt
        .date_naive()
        .with_day(1)
        .expect("day 1 is always valid")
        .and_hms_opt(0, 0, 0)
        .expect("midnight is always valid")
        .and_utc()
        .timestamp() as u64;

    TimeWindow {
        start_of_day,
        start_of_month,
    }
}

impl BudgetEnforcer {
    /// Create a new budget enforcer with the given config and usage log.
    pub fn new(config: BudgetConfig, log: Arc<UsageLog>) -> Self {
        Self { config, log }
    }

    /// Check if a request to the given provider is within budget.
    ///
    /// Returns `Ok(())` if the request is allowed, or
    /// `Err(ProviderError::BudgetExceeded)` with a descriptive message if any
    /// budget limit would be exceeded.
    pub fn check(
        &self,
        provider_name: &str,
        request_type: RequestType,
    ) -> Result<(), ProviderError> {
        let now = symbiotic_core::now_unix();
        let window = compute_time_window(now);

        // 1. Global daily limit check.
        if let Some(global_daily) = self.config.global_daily_limit_usd {
            let filter = UsageFilter {
                since: Some(window.start_of_day),
                ..Default::default()
            };
            let agg = self.log.aggregate(&filter)?;
            let spent = agg.total_cost_usd.unwrap_or(0.0);
            if spent >= global_daily {
                return Err(ProviderError::BudgetExceeded(format!(
                    "global daily limit exceeded: ${spent:.4} of ${global_daily:.2} used"
                )));
            }
        }

        // 2. Per-provider limits.
        if let Some(provider_budget) = self.config.per_provider.get(provider_name) {
            self.check_provider_budget(provider_name, provider_budget, request_type, &window)?;
        }

        Ok(())
    }

    /// Check per-provider budget limits (daily USD, monthly USD, media units,
    /// agent tasks).
    fn check_provider_budget(
        &self,
        provider_name: &str,
        budget: &ProviderBudget,
        request_type: RequestType,
        window: &TimeWindow,
    ) -> Result<(), ProviderError> {
        // Daily USD limit.
        if let Some(daily_limit) = budget.daily_limit_usd {
            let filter = UsageFilter {
                provider: Some(provider_name.to_string()),
                since: Some(window.start_of_day),
                ..Default::default()
            };
            let agg = self.log.aggregate(&filter)?;
            let spent = agg.total_cost_usd.unwrap_or(0.0);
            if spent >= daily_limit {
                return Err(ProviderError::BudgetExceeded(format!(
                    "{provider_name} daily limit exceeded: ${spent:.4} of ${daily_limit:.2} used"
                )));
            }
        }

        // Monthly USD limit.
        if let Some(monthly_limit) = budget.monthly_limit_usd {
            let filter = UsageFilter {
                provider: Some(provider_name.to_string()),
                since: Some(window.start_of_month),
                ..Default::default()
            };
            let agg = self.log.aggregate(&filter)?;
            let spent = agg.total_cost_usd.unwrap_or(0.0);
            if spent >= monthly_limit {
                return Err(ProviderError::BudgetExceeded(format!(
                    "{provider_name} monthly limit exceeded: ${spent:.4} of ${monthly_limit:.2} used"
                )));
            }
        }

        // Media units per day (image/video requests).
        if matches!(
            request_type,
            RequestType::ImageGeneration | RequestType::VideoGeneration
        ) {
            if let Some(max_media) = budget.max_media_units_per_day {
                let filter = UsageFilter {
                    provider: Some(provider_name.to_string()),
                    since: Some(window.start_of_day),
                    request_type: Some(request_type),
                    ..Default::default()
                };
                let agg = self.log.aggregate(&filter)?;
                if agg.total_media_units >= max_media {
                    return Err(ProviderError::BudgetExceeded(format!(
                        "{provider_name} daily media unit limit exceeded: {} of {max_media} used",
                        agg.total_media_units
                    )));
                }
            }
        }

        // Agent tasks per day.
        if request_type == RequestType::AgentTask {
            if let Some(max_tasks) = budget.max_agent_tasks_per_day {
                let filter = UsageFilter {
                    provider: Some(provider_name.to_string()),
                    since: Some(window.start_of_day),
                    request_type: Some(RequestType::AgentTask),
                    ..Default::default()
                };
                let agg = self.log.aggregate(&filter)?;
                if agg.record_count >= max_tasks {
                    return Err(ProviderError::BudgetExceeded(format!(
                        "{provider_name} daily agent task limit exceeded: {} of {max_tasks} used",
                        agg.record_count
                    )));
                }
            }
        }

        Ok(())
    }

    /// Check if spending is approaching the alert threshold for a provider.
    ///
    /// Returns `Some(percent_used)` if the provider's daily spend is at or
    /// above the configured alert threshold percentage, `None` otherwise.
    ///
    /// If no daily limit is configured for the provider, checks the global
    /// daily limit instead. Returns `None` if neither is set.
    pub fn alert_check(&self, provider_name: &str) -> Option<f64> {
        let now = symbiotic_core::now_unix();
        let window = compute_time_window(now);

        // Determine the daily limit and spend to check.
        let (limit, spent) = self.resolve_daily_limit_and_spend(provider_name, &window)?;

        let percent = (spent / limit) * 100.0;
        if percent >= self.config.alert_threshold_percent {
            Some(percent)
        } else {
            None
        }
    }

    /// Resolve the applicable daily limit and current spend for a provider.
    ///
    /// Prefers per-provider daily limit; falls back to global daily limit.
    /// Returns `None` if neither is configured.
    fn resolve_daily_limit_and_spend(
        &self,
        provider_name: &str,
        window: &TimeWindow,
    ) -> Option<(f64, f64)> {
        // Try per-provider first.
        if let Some(budget) = self.config.per_provider.get(provider_name) {
            if let Some(daily_limit) = budget.daily_limit_usd {
                let filter = UsageFilter {
                    provider: Some(provider_name.to_string()),
                    since: Some(window.start_of_day),
                    ..Default::default()
                };
                let agg = self.log.aggregate(&filter).ok()?;
                let spent = agg.total_cost_usd.unwrap_or(0.0);
                return Some((daily_limit, spent));
            }
        }

        // Fall back to global.
        if let Some(global_daily) = self.config.global_daily_limit_usd {
            let filter = UsageFilter {
                since: Some(window.start_of_day),
                ..Default::default()
            };
            let agg = self.log.aggregate(&filter).ok()?;
            let spent = agg.total_cost_usd.unwrap_or(0.0);
            return Some((global_daily, spent));
        }

        None
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{RequestType, UsageRecord};
    use chrono::{Datelike, Timelike};
    use std::collections::HashMap;
    use std::sync::Arc;
    use tempfile::TempDir;

    // -- Helpers ---------------------------------------------------------------

    fn make_log(dir: &TempDir) -> Arc<UsageLog> {
        let path = dir.path().join("usage.ndjson");
        Arc::new(UsageLog::open(&path).unwrap())
    }

    fn make_record(
        provider: &str,
        cost: f64,
        request_type: RequestType,
        media_units: u64,
        ts: u64,
    ) -> UsageRecord {
        UsageRecord {
            provider: provider.to_string(),
            model: "test-model".to_string(),
            timestamp: ts,
            input_tokens: 100,
            output_tokens: 50,
            media_units,
            cost_usd: Some(cost),
            request_type,
            source: "test".to_string(),
            session_id: None,
        }
    }

    fn now() -> u64 {
        symbiotic_core::now_unix()
    }

    fn default_budget() -> BudgetConfig {
        BudgetConfig {
            global_daily_limit_usd: Some(10.0),
            per_provider: HashMap::new(),
            alert_threshold_percent: 80.0,
        }
    }

    // -- check() tests -------------------------------------------------------

    #[test]
    fn check_passes_when_under_global_daily_limit() {
        let dir = TempDir::new().unwrap();
        let log = make_log(&dir);

        // Write a small record for today.
        log.record(&make_record(
            "openai",
            1.0,
            RequestType::Completion,
            0,
            now(),
        ))
        .unwrap();

        let enforcer = BudgetEnforcer::new(default_budget(), log);
        let result = enforcer.check("openai", RequestType::Completion);
        assert!(result.is_ok());
    }

    #[test]
    fn check_fails_when_global_daily_limit_exceeded() {
        let dir = TempDir::new().unwrap();
        let log = make_log(&dir);

        // Write enough spend to exceed the $10 global daily limit.
        log.record(&make_record(
            "openai",
            6.0,
            RequestType::Completion,
            0,
            now(),
        ))
        .unwrap();
        log.record(&make_record(
            "anthropic",
            5.0,
            RequestType::Completion,
            0,
            now(),
        ))
        .unwrap();

        let enforcer = BudgetEnforcer::new(default_budget(), log);
        let result = enforcer.check("openai", RequestType::Completion);
        assert!(result.is_err());

        let err = result.unwrap_err().to_string();
        assert!(err.contains("global daily limit exceeded"), "got: {err}");
    }

    #[test]
    fn check_fails_when_per_provider_daily_limit_exceeded() {
        let dir = TempDir::new().unwrap();
        let log = make_log(&dir);

        log.record(&make_record(
            "openai",
            3.0,
            RequestType::Completion,
            0,
            now(),
        ))
        .unwrap();

        let mut per_provider = HashMap::new();
        per_provider.insert(
            "openai".to_string(),
            ProviderBudget {
                daily_limit_usd: Some(2.0),
                monthly_limit_usd: None,
                max_tokens_per_request: None,
                max_media_units_per_day: None,
                max_agent_tasks_per_day: None,
            },
        );

        let config = BudgetConfig {
            global_daily_limit_usd: Some(100.0), // high enough to not trigger
            per_provider,
            alert_threshold_percent: 80.0,
        };

        let enforcer = BudgetEnforcer::new(config, log);
        let result = enforcer.check("openai", RequestType::Completion);
        assert!(result.is_err());

        let err = result.unwrap_err().to_string();
        assert!(err.contains("openai daily limit exceeded"), "got: {err}");
    }

    #[test]
    fn check_fails_when_per_provider_monthly_limit_exceeded() {
        let dir = TempDir::new().unwrap();
        let log = make_log(&dir);

        // Write a record with a timestamp at the start of the month so it
        // counts in the monthly window.
        let ts = now();
        log.record(&make_record("openai", 50.0, RequestType::Completion, 0, ts))
            .unwrap();

        let mut per_provider = HashMap::new();
        per_provider.insert(
            "openai".to_string(),
            ProviderBudget {
                daily_limit_usd: None,
                monthly_limit_usd: Some(40.0),
                max_tokens_per_request: None,
                max_media_units_per_day: None,
                max_agent_tasks_per_day: None,
            },
        );

        let config = BudgetConfig {
            global_daily_limit_usd: None,
            per_provider,
            alert_threshold_percent: 80.0,
        };

        let enforcer = BudgetEnforcer::new(config, log);
        let result = enforcer.check("openai", RequestType::Completion);
        assert!(result.is_err());

        let err = result.unwrap_err().to_string();
        assert!(err.contains("monthly limit exceeded"), "got: {err}");
    }

    #[test]
    fn check_fails_when_media_units_exceeded() {
        let dir = TempDir::new().unwrap();
        let log = make_log(&dir);

        // 10 media units already used today.
        log.record(&make_record(
            "openai",
            1.0,
            RequestType::ImageGeneration,
            10,
            now(),
        ))
        .unwrap();

        let mut per_provider = HashMap::new();
        per_provider.insert(
            "openai".to_string(),
            ProviderBudget {
                daily_limit_usd: None,
                monthly_limit_usd: None,
                max_tokens_per_request: None,
                max_media_units_per_day: Some(5),
                max_agent_tasks_per_day: None,
            },
        );

        let config = BudgetConfig {
            global_daily_limit_usd: None,
            per_provider,
            alert_threshold_percent: 80.0,
        };

        let enforcer = BudgetEnforcer::new(config, log);
        let result = enforcer.check("openai", RequestType::ImageGeneration);
        assert!(result.is_err());

        let err = result.unwrap_err().to_string();
        assert!(err.contains("media unit limit exceeded"), "got: {err}");
    }

    #[test]
    fn check_passes_media_units_for_non_media_request() {
        let dir = TempDir::new().unwrap();
        let log = make_log(&dir);

        // Media units are over limit, but the request is a Completion, not media.
        log.record(&make_record(
            "openai",
            1.0,
            RequestType::ImageGeneration,
            100,
            now(),
        ))
        .unwrap();

        let mut per_provider = HashMap::new();
        per_provider.insert(
            "openai".to_string(),
            ProviderBudget {
                daily_limit_usd: None,
                monthly_limit_usd: None,
                max_tokens_per_request: None,
                max_media_units_per_day: Some(5),
                max_agent_tasks_per_day: None,
            },
        );

        let config = BudgetConfig {
            global_daily_limit_usd: None,
            per_provider,
            alert_threshold_percent: 80.0,
        };

        let enforcer = BudgetEnforcer::new(config, log);
        // Completion request should pass even though media units are over.
        let result = enforcer.check("openai", RequestType::Completion);
        assert!(result.is_ok());
    }

    #[test]
    fn check_fails_when_agent_tasks_exceeded() {
        let dir = TempDir::new().unwrap();
        let log = make_log(&dir);

        // 5 agent tasks already today.
        for _ in 0..5 {
            log.record(&make_record(
                "claude",
                1.0,
                RequestType::AgentTask,
                0,
                now(),
            ))
            .unwrap();
        }

        let mut per_provider = HashMap::new();
        per_provider.insert(
            "claude".to_string(),
            ProviderBudget {
                daily_limit_usd: None,
                monthly_limit_usd: None,
                max_tokens_per_request: None,
                max_media_units_per_day: None,
                max_agent_tasks_per_day: Some(5),
            },
        );

        let config = BudgetConfig {
            global_daily_limit_usd: None,
            per_provider,
            alert_threshold_percent: 80.0,
        };

        let enforcer = BudgetEnforcer::new(config, log);
        let result = enforcer.check("claude", RequestType::AgentTask);
        assert!(result.is_err());

        let err = result.unwrap_err().to_string();
        assert!(err.contains("agent task limit exceeded"), "got: {err}");
    }

    #[test]
    fn check_passes_with_no_budget_configured() {
        let dir = TempDir::new().unwrap();
        let log = make_log(&dir);

        log.record(&make_record(
            "openai",
            999.0,
            RequestType::Completion,
            0,
            now(),
        ))
        .unwrap();

        let config = BudgetConfig {
            global_daily_limit_usd: None,
            per_provider: HashMap::new(),
            alert_threshold_percent: 80.0,
        };

        let enforcer = BudgetEnforcer::new(config, log);
        let result = enforcer.check("openai", RequestType::Completion);
        assert!(result.is_ok());
    }

    #[test]
    fn check_ignores_old_records() {
        let dir = TempDir::new().unwrap();
        let log = make_log(&dir);

        // Write a large record from yesterday (well before start-of-day).
        let yesterday = now() - 86_400 * 2;
        log.record(&make_record(
            "openai",
            100.0,
            RequestType::Completion,
            0,
            yesterday,
        ))
        .unwrap();

        let config = BudgetConfig {
            global_daily_limit_usd: Some(10.0),
            per_provider: HashMap::new(),
            alert_threshold_percent: 80.0,
        };

        let enforcer = BudgetEnforcer::new(config, log);
        let result = enforcer.check("openai", RequestType::Completion);
        assert!(
            result.is_ok(),
            "old records should not count against daily limit"
        );
    }

    #[test]
    fn check_unknown_provider_skips_per_provider_limits() {
        let dir = TempDir::new().unwrap();
        let log = make_log(&dir);

        log.record(&make_record(
            "unknown",
            5.0,
            RequestType::Completion,
            0,
            now(),
        ))
        .unwrap();

        let mut per_provider = HashMap::new();
        per_provider.insert(
            "openai".to_string(),
            ProviderBudget {
                daily_limit_usd: Some(1.0),
                monthly_limit_usd: None,
                max_tokens_per_request: None,
                max_media_units_per_day: None,
                max_agent_tasks_per_day: None,
            },
        );

        let config = BudgetConfig {
            global_daily_limit_usd: Some(100.0),
            per_provider,
            alert_threshold_percent: 80.0,
        };

        let enforcer = BudgetEnforcer::new(config, log);
        // "unknown" has no per-provider limits, so should pass.
        let result = enforcer.check("unknown", RequestType::Completion);
        assert!(result.is_ok());
    }

    // -- alert_check() tests -------------------------------------------------

    #[test]
    fn alert_check_returns_percent_when_above_threshold() {
        let dir = TempDir::new().unwrap();
        let log = make_log(&dir);

        // $9 of $10 = 90%, threshold is 80%.
        log.record(&make_record(
            "openai",
            9.0,
            RequestType::Completion,
            0,
            now(),
        ))
        .unwrap();

        let config = BudgetConfig {
            global_daily_limit_usd: Some(10.0),
            per_provider: HashMap::new(),
            alert_threshold_percent: 80.0,
        };

        let enforcer = BudgetEnforcer::new(config, log);
        let result = enforcer.alert_check("openai");
        assert!(result.is_some());
        let percent = result.unwrap();
        assert!((percent - 90.0).abs() < 0.1, "expected ~90%, got {percent}");
    }

    #[test]
    fn alert_check_returns_none_when_below_threshold() {
        let dir = TempDir::new().unwrap();
        let log = make_log(&dir);

        // $3 of $10 = 30%, threshold is 80%.
        log.record(&make_record(
            "openai",
            3.0,
            RequestType::Completion,
            0,
            now(),
        ))
        .unwrap();

        let config = BudgetConfig {
            global_daily_limit_usd: Some(10.0),
            per_provider: HashMap::new(),
            alert_threshold_percent: 80.0,
        };

        let enforcer = BudgetEnforcer::new(config, log);
        let result = enforcer.alert_check("openai");
        assert!(result.is_none());
    }

    #[test]
    fn alert_check_prefers_per_provider_limit() {
        let dir = TempDir::new().unwrap();
        let log = make_log(&dir);

        // $4.5 of $5 per-provider limit = 90%.
        log.record(&make_record(
            "openai",
            4.5,
            RequestType::Completion,
            0,
            now(),
        ))
        .unwrap();

        let mut per_provider = HashMap::new();
        per_provider.insert(
            "openai".to_string(),
            ProviderBudget {
                daily_limit_usd: Some(5.0),
                monthly_limit_usd: None,
                max_tokens_per_request: None,
                max_media_units_per_day: None,
                max_agent_tasks_per_day: None,
            },
        );

        let config = BudgetConfig {
            global_daily_limit_usd: Some(100.0), // high, should not trigger
            per_provider,
            alert_threshold_percent: 80.0,
        };

        let enforcer = BudgetEnforcer::new(config, log);
        let result = enforcer.alert_check("openai");
        assert!(result.is_some());
        let percent = result.unwrap();
        assert!((percent - 90.0).abs() < 0.1, "expected ~90%, got {percent}");
    }

    #[test]
    fn alert_check_returns_none_when_no_limits() {
        let dir = TempDir::new().unwrap();
        let log = make_log(&dir);

        let config = BudgetConfig {
            global_daily_limit_usd: None,
            per_provider: HashMap::new(),
            alert_threshold_percent: 80.0,
        };

        let enforcer = BudgetEnforcer::new(config, log);
        let result = enforcer.alert_check("openai");
        assert!(result.is_none());
    }

    // -- compute_time_window tests -------------------------------------------

    #[test]
    fn time_window_start_of_day_is_midnight_utc() {
        // 2026-02-08 12:34:56 UTC
        let ts = 1770504896;
        let window = compute_time_window(ts);

        // Start of day should be 2026-02-08 00:00:00 UTC.
        let dt = Utc.timestamp_opt(window.start_of_day as i64, 0).unwrap();
        assert_eq!(dt.hour(), 0);
        assert_eq!(dt.minute(), 0);
        assert_eq!(dt.second(), 0);
        assert!(window.start_of_day <= ts);
        assert!(window.start_of_day + 86400 > ts);
    }

    #[test]
    fn time_window_start_of_month_is_first_day() {
        // 2026-02-08 12:34:56 UTC
        let ts = 1770504896;
        let window = compute_time_window(ts);

        // Start of month should be 2026-02-01 00:00:00 UTC.
        let dt = Utc.timestamp_opt(window.start_of_month as i64, 0).unwrap();
        assert_eq!(dt.day(), 1);
        assert_eq!(dt.hour(), 0);
        assert!(window.start_of_month <= ts);
    }
}
