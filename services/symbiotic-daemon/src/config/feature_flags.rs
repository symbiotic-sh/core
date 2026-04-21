//! Env-backed runtime feature flag readers.
//!
//! Flags land here so every subsystem consults a single source of truth and
//! tests can drive state deterministically by manipulating env vars. Readers
//! are intentionally cheap (no caching, no parse once) — flags are checked
//! off the hot path (per-goal, per-agent-spawn) and we value test isolation
//! over sub-microsecond latency.

// NOTE: `GoalProcess` (T130 §04) has not yet landed a public shape that is
// stable for §03 to depend on. The batch-inquisitor gate therefore takes a
// best-effort `goal_id`-shaped argument so later phases can pass a richer
// `GoalProcess` reference without an API break. The env-var is the MVP gate;
// per-goal overrides will layer on top in a later chunk.

/// Returns `true` when the `SYMBIOTIC_BATCH_INQUISITOR` flag is active.
///
/// Accepts `1`, `true`, `yes`, `on` (case-insensitive) as truthy. Any other
/// value — including unset — returns `false`.
///
/// # Arguments
///
/// * `goal_id_hint` — optional goal identifier. Reserved for the per-goal
///   override mechanism landing with T130 §04; currently unused so callers
///   can adopt the richer signature now without waiting.
pub fn batch_inquisitor_enabled(goal_id_hint: Option<&str>) -> bool {
    // Silence unused-var lint while keeping the signature future-proof.
    let _ = goal_id_hint;

    std::env::var("SYMBIOTIC_BATCH_INQUISITOR")
        .map(|value| is_truthy(&value))
        .unwrap_or(false)
}

fn is_truthy(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    // These tests mutate `SYMBIOTIC_BATCH_INQUISITOR` on the process env. We
    // use a module-wide `Mutex` to serialise them so parallel test execution
    // cannot race on the shared env. `std::env::set_var` is otherwise sound
    // enough for this narrow testing context.
    use std::sync::Mutex;
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn scoped<T>(value: Option<&str>, body: impl FnOnce() -> T) -> T {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var("SYMBIOTIC_BATCH_INQUISITOR").ok();
        match value {
            Some(v) => std::env::set_var("SYMBIOTIC_BATCH_INQUISITOR", v),
            None => std::env::remove_var("SYMBIOTIC_BATCH_INQUISITOR"),
        }
        let out = body();
        match prev {
            Some(v) => std::env::set_var("SYMBIOTIC_BATCH_INQUISITOR", v),
            None => std::env::remove_var("SYMBIOTIC_BATCH_INQUISITOR"),
        }
        out
    }

    #[test]
    fn unset_returns_false() {
        scoped(None, || {
            assert!(!batch_inquisitor_enabled(None));
        });
    }

    #[test]
    fn truthy_values_enable_flag() {
        for v in ["1", "true", "TRUE", "yes", "On"] {
            scoped(Some(v), || {
                assert!(
                    batch_inquisitor_enabled(None),
                    "expected `{v}` to be truthy"
                );
            });
        }
    }

    #[test]
    fn other_values_disable_flag() {
        for v in ["0", "false", "no", "off", "", "   ", "maybe"] {
            scoped(Some(v), || {
                assert!(
                    !batch_inquisitor_enabled(None),
                    "expected `{v}` to be falsy"
                );
            });
        }
    }

    #[test]
    fn goal_id_hint_is_currently_ignored() {
        scoped(Some("1"), || {
            assert!(batch_inquisitor_enabled(Some("goal-xyz")));
        });
        scoped(None, || {
            assert!(!batch_inquisitor_enabled(Some("goal-xyz")));
        });
    }
}
