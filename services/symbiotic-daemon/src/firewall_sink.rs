//! Daemon-side wiring for the Content Firewall (T132 §05).
//!
//! Implements the [`QuarantineSink`] trait declared in `symbiotic-intake`:
//! firewall-rejected ingest content writes to a JSONL audit log under
//! `knowledge-base/self/audit/firewall_quarantine.jsonl` per design §4, and
//! emits a `firewall.quarantine` Matrix event so the operator is notified
//! through the same surface used for other operator alerts.
//!
//! Per design §4 the JSONL log records only:
//! - timestamp + source kind / ref
//! - SHA-256 content hash + first 64 chars prefix (never the full payload)
//! - the firewall verdict (with quarantine class + findings)
//!
//! Storing the full content would itself become a prompt-injection vector.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

use anyhow::{Context, Result};
use serde::Serialize;
use symbiotic_intake::{QuarantineRecord, QuarantineSink};
use time::OffsetDateTime;

/// Channel used to fan a quarantine event out to the Matrix transport.
///
/// The daemon hands the receiving end to its existing outbound matrix
/// dispatcher; this trait keeps the sink decoupled from the concrete
/// transport so unit tests can substitute a no-op.
pub trait QuarantineAlertChannel: Send + Sync {
    fn alert(&self, payload: serde_json::Value) -> Result<()>;
}

/// Default no-op channel for tests.
pub struct NoopAlertChannel;

impl QuarantineAlertChannel for NoopAlertChannel {
    fn alert(&self, _payload: serde_json::Value) -> Result<()> {
        Ok(())
    }
}

/// Production sink: appends a JSONL line to the audit log + emits a Matrix
/// alert via [`QuarantineAlertChannel`].
pub struct JsonlQuarantineSink {
    log_path: PathBuf,
    alerts: Box<dyn QuarantineAlertChannel>,
    write_lock: Mutex<()>,
}

impl JsonlQuarantineSink {
    /// Construct a sink that writes to `kb_root/self/audit/firewall_quarantine.jsonl`.
    pub fn new(kb_root: &std::path::Path, alerts: Box<dyn QuarantineAlertChannel>) -> Result<Self> {
        let log_path = kb_root
            .join("self")
            .join("audit")
            .join("firewall_quarantine.jsonl");
        if let Some(parent) = log_path.parent() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!("failed to create firewall audit dir {}", parent.display())
            })?;
        }
        Ok(Self {
            log_path,
            alerts,
            write_lock: Mutex::new(()),
        })
    }

    /// Sink with no Matrix alerts (used by integration tests that only assert
    /// on the JSONL log).
    pub fn audit_only(kb_root: &std::path::Path) -> Result<Self> {
        Self::new(kb_root, Box::new(NoopAlertChannel))
    }

    pub fn log_path(&self) -> &std::path::Path {
        &self.log_path
    }
}

#[derive(Debug, Serialize)]
struct AuditRow<'a> {
    #[serde(with = "time::serde::rfc3339")]
    ts: OffsetDateTime,
    source_kind: &'a str,
    source_ref: Option<&'a str>,
    content_hash: &'a str,
    prefix: &'a str,
    verdict: &'a symbiotic_firewall::types::FirewallVerdict,
}

impl QuarantineSink for JsonlQuarantineSink {
    fn record(&self, record: QuarantineRecord) -> Result<()> {
        let row = AuditRow {
            ts: OffsetDateTime::now_utc(),
            source_kind: &record.source_kind,
            source_ref: record.source_ref.as_deref(),
            content_hash: &record.content_hash,
            prefix: &record.prefix,
            verdict: &record.verdict,
        };
        let line = serde_json::to_string(&row).context("serialize quarantine row")?;
        {
            let _g = self
                .write_lock
                .lock()
                .map_err(|_| anyhow::anyhow!("quarantine sink lock poisoned"))?;
            let mut f = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.log_path)
                .with_context(|| {
                    format!(
                        "failed to open firewall audit log {}",
                        self.log_path.display()
                    )
                })?;
            writeln!(f, "{line}")
                .with_context(|| format!("write firewall audit log {}", self.log_path.display()))?;
        }

        // Emit Matrix alert: digest-only fields, no raw content.
        let alert_payload = serde_json::json!({
            "source_kind": record.source_kind,
            "source_ref": record.source_ref,
            "content_hash": record.content_hash,
            "quarantine_class": record.verdict.quarantine_class,
            "verdict_version": record.verdict.verdict_version,
        });
        let _ = self.alerts.alert(alert_payload);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use symbiotic_archive::trusted_skip_verdict;
    use symbiotic_firewall::types::{QuarantineClass, Verdict};
    use tempfile::tempdir;

    fn quarantined_verdict() -> symbiotic_firewall::types::FirewallVerdict {
        let mut v = trusted_skip_verdict();
        v.verdict = Verdict::Quarantined;
        v.quarantine_class = Some(QuarantineClass::SecurityRisk);
        v
    }

    #[test]
    fn audit_only_writes_jsonl_line() {
        let dir = tempdir().expect("tmpdir");
        let sink = JsonlQuarantineSink::audit_only(dir.path()).expect("sink");
        sink.record(QuarantineRecord {
            source_kind: "intake.url".into(),
            source_ref: Some("https://evil.example/inject".into()),
            content_hash: "deadbeef".into(),
            prefix: "ignore previous".into(),
            verdict: quarantined_verdict(),
        })
        .expect("record");
        let log = std::fs::read_to_string(sink.log_path()).expect("read log");
        assert!(log.contains("intake.url"));
        assert!(log.contains("deadbeef"));
        assert!(log.contains("security_risk"));
    }

    #[test]
    fn alert_channel_receives_digest_payload() {
        struct CapturingChannel {
            captured: std::sync::Mutex<Option<serde_json::Value>>,
        }
        impl QuarantineAlertChannel for CapturingChannel {
            fn alert(&self, payload: serde_json::Value) -> Result<()> {
                *self.captured.lock().unwrap() = Some(payload);
                Ok(())
            }
        }
        let dir = tempdir().expect("tmpdir");
        let chan = std::sync::Arc::new(CapturingChannel {
            captured: std::sync::Mutex::new(None),
        });
        // Wrap chan via a forwarding Box.
        struct Forward(std::sync::Arc<CapturingChannel>);
        impl QuarantineAlertChannel for Forward {
            fn alert(&self, payload: serde_json::Value) -> Result<()> {
                self.0.alert(payload)
            }
        }
        let sink =
            JsonlQuarantineSink::new(dir.path(), Box::new(Forward(chan.clone()))).expect("sink");
        sink.record(QuarantineRecord {
            source_kind: "tool.observation".into(),
            source_ref: None,
            content_hash: "cafebabe".into(),
            prefix: "{...}".into(),
            verdict: quarantined_verdict(),
        })
        .expect("record");
        let captured = chan.captured.lock().unwrap().clone().expect("alert sent");
        assert_eq!(
            captured.get("content_hash").and_then(|v| v.as_str()),
            Some("cafebabe")
        );
        assert_eq!(
            captured.get("source_kind").and_then(|v| v.as_str()),
            Some("tool.observation")
        );
    }
}
