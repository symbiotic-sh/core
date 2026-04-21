//! Stage A — structural sanitization — fixture-driven tests.
//!
//! Each fixture file under `tests/fixtures/` is classified by directory:
//!
//! - `safe/` — must pass Stage A cleanly (no findings beyond informational).
//! - `stage_a_fail/` — must produce a `Quarantined` verdict with
//!   `QuarantineClass::SourceIntegrity`.
//!
//! The fixture's file extension selects the claimed content-type.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use symbiotic_firewall::stages::{run_stage_a, StageAConfig, StageAOutcome};
use symbiotic_firewall::{
    CallSite, ConsumingAgentScope, ContentSource, QuarantineClass, ScanContext, Verdict,
};
use time::OffsetDateTime;

fn fixtures_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn claimed_mime_for(path: &Path) -> Option<String> {
    let ext = path.extension().and_then(|e| e.to_str())?;
    Some(match ext {
        "html" => "text/html; charset=utf-8".into(),
        "md" => "text/markdown".into(),
        "txt" => "text/plain".into(),
        "json" => "application/json".into(),
        _ => return None,
    })
}

fn ctx_for(path: &Path) -> ScanContext {
    ScanContext {
        source: ContentSource {
            kind: "web_fetch".into(),
            url: Some(format!("file://{}", path.display())),
            fetched_at: OffsetDateTime::now_utc(),
            claimed_content_type: claimed_mime_for(path),
            headers: BTreeMap::new(),
        },
        consuming_agent_scope: ConsumingAgentScope::minimal("test-agent"),
        call_site: CallSite::new("test.stage_a"),
    }
}

fn read_fixture(path: &Path) -> String {
    std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("read fixture {}: {}", path.display(), e))
}

fn list_fixtures(subdir: &str) -> Vec<PathBuf> {
    let dir = fixtures_root().join(subdir);
    let mut out = Vec::new();
    for entry in
        std::fs::read_dir(&dir).unwrap_or_else(|e| panic!("read_dir {}: {}", dir.display(), e))
    {
        let path = entry.expect("dir entry").path();
        if path.is_file() {
            out.push(path);
        }
    }
    out.sort();
    out
}

#[test]
fn every_safe_fixture_passes_stage_a() {
    let cfg = StageAConfig::default();
    let mut ran = 0;
    for path in list_fixtures("safe") {
        let payload = read_fixture(&path);
        let ctx = ctx_for(&path);
        match run_stage_a(&ctx, &payload, &cfg) {
            StageAOutcome::Passed { cleaned, findings } => {
                // Safe fixtures MUST not produce any structural-violation
                // findings. (Allowlist-tagged external image finds are OK
                // only if the fixture explicitly tests that — none of ours
                // do right now, so assert none.)
                assert!(
                    findings.is_empty(),
                    "{}: unexpected findings: {:?}",
                    path.display(),
                    findings
                );
                assert!(!cleaned.is_empty(), "{}: cleaned empty", path.display());
            }
            StageAOutcome::Quarantined(v) => {
                panic!(
                    "{}: unexpected quarantine (annotations: {:?})",
                    path.display(),
                    v.annotations
                );
            }
        }
        ran += 1;
    }
    assert!(ran > 0, "no safe fixtures found");
}

#[test]
fn every_stage_a_fail_fixture_quarantines() {
    let cfg = StageAConfig::default();
    let mut ran = 0;
    for path in list_fixtures("stage_a_fail") {
        let payload = read_fixture(&path);
        let ctx = ctx_for(&path);
        match run_stage_a(&ctx, &payload, &cfg) {
            StageAOutcome::Quarantined(v) => {
                assert_eq!(v.verdict, Verdict::Quarantined);
                assert_eq!(
                    v.quarantine_class,
                    Some(QuarantineClass::SourceIntegrity),
                    "{}: wrong quarantine class",
                    path.display()
                );
                assert!(
                    !v.annotations.is_empty(),
                    "{}: quarantine without annotations",
                    path.display()
                );
            }
            StageAOutcome::Passed { findings, .. } => {
                panic!(
                    "{}: expected quarantine, got pass (findings: {:?})",
                    path.display(),
                    findings
                );
            }
        }
        ran += 1;
    }
    assert!(ran > 0, "no stage_a_fail fixtures found");
}

#[test]
fn oversized_payload_quarantines() {
    let ctx = ctx_for(Path::new("over.txt"));
    let payload = "a".repeat(2 * 1024 * 1024); // 2 MiB, over default 1 MiB
    match run_stage_a(&ctx, &payload, &StageAConfig::default()) {
        StageAOutcome::Quarantined(v) => {
            assert!(v
                .annotations
                .iter()
                .any(|f| f.detail.contains("exceeds limit")));
        }
        other => panic!("expected quarantine: {other:?}"),
    }
}

#[test]
fn stage_a_is_deterministic() {
    // Same input -> same verdict (modulo timestamp, which is elided here).
    let path = fixtures_root().join("stage_a_fail/script_tag.html");
    let payload = read_fixture(&path);
    let ctx = ctx_for(&path);
    let cfg = StageAConfig::default();
    let a = run_stage_a(&ctx, &payload, &cfg);
    let b = run_stage_a(&ctx, &payload, &cfg);
    match (a, b) {
        (StageAOutcome::Quarantined(va), StageAOutcome::Quarantined(vb)) => {
            assert_eq!(va.verdict, vb.verdict);
            assert_eq!(va.quarantine_class, vb.quarantine_class);
            assert_eq!(va.annotations.len(), vb.annotations.len());
        }
        _ => panic!("expected both quarantined"),
    }
}

#[test]
fn random_bytes_do_not_panic() {
    // Deterministic pseudo-random: xorshift.
    let mut state: u64 = 0xDEADBEEF;
    for _ in 0..200 {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        let len = ((state % 4096) as usize) + 1;
        // Build a UTF-8 safe string from ASCII printable range.
        let payload: String = (0..len)
            .map(|i| {
                let b = (state.wrapping_add(i as u64) % 95) as u8 + 32; // 32..127
                b as char
            })
            .collect();
        let ctx = ctx_for(Path::new("fuzz.txt"));
        // Just assert no panic.
        let _ = run_stage_a(&ctx, &payload, &StageAConfig::default());
    }
}
