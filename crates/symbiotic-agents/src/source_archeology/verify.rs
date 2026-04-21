//! Stage 6 — Verify (patch applicability + lint).
//!
//! Runs per-finding checks on `Resolve`-disposition decisions from
//! Triage. Rejections downgrade the decision to `Defer` with the
//! rejection reason appended to the rationale. Other dispositions
//! (`Defer`, `Escalate`) pass through unverified.
//!
//! See `docs/design/source-archeology.md` §Stage 6 — Verify.

use std::path::Path;
use std::process::Command;

use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use super::archeology_types::{Finding, FindingAction, FindingDisposition, TriageDecision};

// ── Types ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerifyOutcome {
    Accepted,
    Rejected { reason: String },
}

// ── Traits ─────────────────────────────────────────────────────────────

#[async_trait]
pub trait PatchVerifier: Send + Sync {
    /// Check whether `diff` applies cleanly against `clone_root`.
    async fn verify(&self, clone_root: &Path, diff: &str) -> Result<VerifyOutcome>;
}

#[async_trait]
pub trait LintVerifier: Send + Sync {
    /// Check a file's content for lint violations.
    async fn verify(&self, path: &str, content: &str) -> Result<VerifyOutcome>;
}

// ── Default impls ──────────────────────────────────────────────────────

/// Default patch verifier: `git apply --check` in `clone_root`.
pub struct GitApplyCheckVerifier;

#[async_trait]
impl PatchVerifier for GitApplyCheckVerifier {
    async fn verify(&self, clone_root: &Path, diff: &str) -> Result<VerifyOutcome> {
        use std::io::Write;
        let mut child = Command::new("git")
            .arg("-C")
            .arg(clone_root)
            .args(["apply", "--check", "-"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()?;
        if let Some(stdin) = child.stdin.as_mut() {
            stdin.write_all(diff.as_bytes())?;
        }
        let output = child.wait_with_output()?;
        if output.status.success() {
            Ok(VerifyOutcome::Accepted)
        } else {
            Ok(VerifyOutcome::Rejected {
                reason: format!(
                    "git apply --check failed: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                ),
            })
        }
    }
}

/// No-op lint verifier: always accepts. Useful as a test double or when
/// the operator has opted out of lint entirely via `VerifyConfig`.
pub struct NoopLintVerifier;

#[async_trait]
impl LintVerifier for NoopLintVerifier {
    async fn verify(&self, _: &str, _: &str) -> Result<VerifyOutcome> {
        Ok(VerifyOutcome::Accepted)
    }
}

/// Markdownlint-backed verifier. Writes the candidate content to a
/// tempfile and invokes the configured `markdownlint` binary against it;
/// non-zero exit with stderr → `Rejected`.
///
/// If the binary isn't available at construction time, falls back to
/// `VerifyOutcome::Accepted` (same as `NoopLintVerifier`) rather than
/// erroring — keeps the pipeline working on deploys that haven't
/// installed a Node runtime. `with_binary()` accepts a custom path
/// (useful for tests pointing at a shell-script stub); `autodiscover()`
/// searches `PATH` for common variants.
pub struct MarkdownlintVerifier {
    /// Absolute path to the markdownlint binary, or `None` to no-op.
    binary: Option<std::path::PathBuf>,
}

impl MarkdownlintVerifier {
    /// Use a specific binary path. No existence check — caller
    /// guarantees.
    pub fn with_binary(binary: impl Into<std::path::PathBuf>) -> Self {
        Self {
            binary: Some(binary.into()),
        }
    }

    /// Probe `PATH` for a known markdownlint variant. Order:
    /// `markdownlint-cli2` (preferred, faster), then `markdownlint`.
    /// Returns a verifier that no-ops if nothing is found.
    pub fn autodiscover() -> Self {
        let candidates = ["markdownlint-cli2", "markdownlint"];
        let binary = candidates.iter().find_map(|name| which_in_path(name));
        Self { binary }
    }

    /// True if this verifier will actually invoke a binary. `false`
    /// means the verifier auto-no-ops.
    pub fn is_active(&self) -> bool {
        self.binary.is_some()
    }
}

#[async_trait]
impl LintVerifier for MarkdownlintVerifier {
    async fn verify(&self, path: &str, content: &str) -> Result<VerifyOutcome> {
        let Some(binary) = &self.binary else {
            return Ok(VerifyOutcome::Accepted);
        };

        // Write content to a tempfile so markdownlint has a file to
        // read. Use the original path's extension as a hint.
        let tmp_dir = tempfile::tempdir()?;
        let filename = std::path::Path::new(path)
            .file_name()
            .map(|n| n.to_os_string())
            .unwrap_or_else(|| std::ffi::OsString::from("content.md"));
        let tmp_path = tmp_dir.path().join(&filename);
        std::fs::write(&tmp_path, content)?;

        let output = Command::new(binary).arg(&tmp_path).output()?;
        if output.status.success() {
            Ok(VerifyOutcome::Accepted)
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stdout = String::from_utf8_lossy(&output.stdout);
            let combined = format!("{stderr}{stdout}").trim().to_string();
            Ok(VerifyOutcome::Rejected {
                reason: format!(
                    "markdownlint failed for {path}: {}",
                    if combined.is_empty() {
                        "(no output)".to_string()
                    } else {
                        combined
                    }
                ),
            })
        }
    }
}

/// Minimal `which`-equivalent: check every `PATH` segment for an
/// executable file named `cmd`. Returns the first match. No shim
/// resolution, no wildcard expansion — the markdownlint binary is
/// installed by name, this is sufficient.
fn which_in_path(cmd: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(cmd);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

// ── Config (per docs/design/agent-tunables.md) ─────────────────────────

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct VerifyConfig {
    /// Whether to run the lint verifier on scaffold `NewFile` findings.
    /// Default: true.
    pub lint_scaffold_files: bool,
}

impl Default for VerifyConfig {
    fn default() -> Self {
        Self {
            lint_scaffold_files: true,
        }
    }
}

// ── Runner ─────────────────────────────────────────────────────────────

pub async fn run(
    findings: &[Finding],
    decisions: Vec<TriageDecision>,
    clone_root: &Path,
    patch_verifier: &dyn PatchVerifier,
    lint_verifier: &dyn LintVerifier,
    config: VerifyConfig,
) -> Result<Vec<TriageDecision>> {
    let mut out: Vec<TriageDecision> = Vec::with_capacity(decisions.len());
    for mut decision in decisions {
        if decision.disposition != FindingDisposition::Resolve {
            out.push(decision);
            continue;
        }
        let Some(finding) = findings.iter().find(|f| f.id == decision.finding_id) else {
            // Orphan decision — no matching finding. Pass through; the
            // caller presumably has a reason. Not this stage's concern.
            out.push(decision);
            continue;
        };

        let outcome = match &finding.proposed_action {
            FindingAction::Patch { diff } => match patch_verifier.verify(clone_root, diff).await {
                Ok(o) => o,
                Err(err) => VerifyOutcome::Rejected {
                    reason: format!("verify_error: {err}"),
                },
            },
            FindingAction::NewFile { path, content } => {
                if config.lint_scaffold_files {
                    match lint_verifier.verify(path, content).await {
                        Ok(o) => o,
                        Err(err) => VerifyOutcome::Rejected {
                            reason: format!("verify_error: {err}"),
                        },
                    }
                } else {
                    VerifyOutcome::Accepted
                }
            }
            // Report findings don't reach Resolve in practice — Handoff
            // wraps them as escalate-ish content. Defensive pass-through.
            FindingAction::Report { .. } => VerifyOutcome::Accepted,
        };

        if let VerifyOutcome::Rejected { reason } = outcome {
            decision.disposition = FindingDisposition::Defer;
            decision.rationale = format!(
                "verify_rejected={reason}; previous_disposition=resolve; previous_rationale={}",
                decision.rationale
            );
        }
        out.push(decision);
    }
    Ok(out)
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source_archeology::archeology_types::{
        FindingAction, FindingSeverity, FindingSourceStage, GoalAlignment,
    };
    use crate::source_archeology::fixtures::{ScriptedLintVerifier, ScriptedPatchVerifier};
    use std::path::PathBuf;

    fn finding_patch(id: &str) -> Finding {
        Finding {
            id: id.to_string(),
            source_stage: FindingSourceStage::Reconcile,
            severity: FindingSeverity::Medium,
            category: "drift".to_string(),
            evidence_path: "docs/x.md".to_string(),
            description: "d".to_string(),
            proposed_action: FindingAction::Patch {
                diff: "--- a\n+++ b\n@@ -1 +1 @@\n-a\n+b\n".to_string(),
            },
            goal_alignment: GoalAlignment::InScope,
        }
    }

    fn finding_newfile(id: &str) -> Finding {
        Finding {
            id: id.to_string(),
            source_stage: FindingSourceStage::Scaffold,
            severity: FindingSeverity::Medium,
            category: "scaffold_new_file".to_string(),
            evidence_path: "README.md".to_string(),
            description: "d".to_string(),
            proposed_action: FindingAction::NewFile {
                path: "README.md".to_string(),
                content: "# test\n".to_string(),
            },
            goal_alignment: GoalAlignment::InScope,
        }
    }

    fn finding_report(id: &str) -> Finding {
        Finding {
            id: id.to_string(),
            source_stage: FindingSourceStage::Reconcile,
            severity: FindingSeverity::Low,
            category: "note".to_string(),
            evidence_path: "".to_string(),
            description: "d".to_string(),
            proposed_action: FindingAction::Report {
                text: "hi".to_string(),
            },
            goal_alignment: GoalAlignment::InScope,
        }
    }

    fn decision(id: &str, disposition: FindingDisposition) -> TriageDecision {
        TriageDecision {
            finding_id: id.to_string(),
            disposition,
            rationale: "from-triage".to_string(),
        }
    }

    #[tokio::test]
    async fn patch_accepted_preserves_resolve() {
        let findings = vec![finding_patch("f1")];
        let decisions = vec![decision("f1", FindingDisposition::Resolve)];
        let patch = ScriptedPatchVerifier::new_accept();
        let lint = ScriptedLintVerifier::new_accept();
        let out = run(
            &findings,
            decisions,
            &PathBuf::from("/tmp"),
            &patch,
            &lint,
            VerifyConfig::default(),
        )
        .await
        .unwrap();
        assert_eq!(out[0].disposition, FindingDisposition::Resolve);
        assert_eq!(out[0].rationale, "from-triage");
    }

    #[tokio::test]
    async fn patch_rejected_flips_to_defer() {
        let findings = vec![finding_patch("f1")];
        let decisions = vec![decision("f1", FindingDisposition::Resolve)];
        let patch = ScriptedPatchVerifier::new_reject("patch does not apply");
        let lint = ScriptedLintVerifier::new_accept();
        let out = run(
            &findings,
            decisions,
            &PathBuf::from("/tmp"),
            &patch,
            &lint,
            VerifyConfig::default(),
        )
        .await
        .unwrap();
        assert_eq!(out[0].disposition, FindingDisposition::Defer);
        assert!(out[0]
            .rationale
            .contains("verify_rejected=patch does not apply"));
        assert!(out[0].rationale.contains("previous_disposition=resolve"));
    }

    #[tokio::test]
    async fn newfile_accepted_preserves_resolve() {
        let findings = vec![finding_newfile("f1")];
        let decisions = vec![decision("f1", FindingDisposition::Resolve)];
        let patch = ScriptedPatchVerifier::new_accept();
        let lint = ScriptedLintVerifier::new_accept();
        let out = run(
            &findings,
            decisions,
            &PathBuf::from("/tmp"),
            &patch,
            &lint,
            VerifyConfig::default(),
        )
        .await
        .unwrap();
        assert_eq!(out[0].disposition, FindingDisposition::Resolve);
    }

    #[tokio::test]
    async fn newfile_lint_rejected_flips_to_defer() {
        let findings = vec![finding_newfile("f1")];
        let decisions = vec![decision("f1", FindingDisposition::Resolve)];
        let patch = ScriptedPatchVerifier::new_accept();
        let lint = ScriptedLintVerifier::new_reject("MD013 line too long");
        let out = run(
            &findings,
            decisions,
            &PathBuf::from("/tmp"),
            &patch,
            &lint,
            VerifyConfig::default(),
        )
        .await
        .unwrap();
        assert_eq!(out[0].disposition, FindingDisposition::Defer);
        assert!(out[0].rationale.contains("MD013 line too long"));
    }

    #[tokio::test]
    async fn non_resolve_dispositions_pass_through() {
        let findings = vec![finding_patch("f1"), finding_newfile("f2")];
        let decisions = vec![
            decision("f1", FindingDisposition::Defer),
            decision("f2", FindingDisposition::Escalate),
        ];
        // Use a rejecting verifier to prove it's NOT invoked on
        // non-Resolve decisions (rationale would change if it were).
        let patch = ScriptedPatchVerifier::new_reject("would fail");
        let lint = ScriptedLintVerifier::new_reject("would fail");
        let out = run(
            &findings,
            decisions,
            &PathBuf::from("/tmp"),
            &patch,
            &lint,
            VerifyConfig::default(),
        )
        .await
        .unwrap();
        assert_eq!(out[0].disposition, FindingDisposition::Defer);
        assert_eq!(out[0].rationale, "from-triage");
        assert_eq!(out[1].disposition, FindingDisposition::Escalate);
        assert_eq!(out[1].rationale, "from-triage");
    }

    #[tokio::test]
    async fn verifier_error_flips_to_defer() {
        let findings = vec![finding_patch("f1")];
        let decisions = vec![decision("f1", FindingDisposition::Resolve)];
        let patch = ScriptedPatchVerifier::new_err();
        let lint = ScriptedLintVerifier::new_accept();
        let out = run(
            &findings,
            decisions,
            &PathBuf::from("/tmp"),
            &patch,
            &lint,
            VerifyConfig::default(),
        )
        .await
        .unwrap();
        assert_eq!(out[0].disposition, FindingDisposition::Defer);
        assert!(out[0].rationale.contains("verify_error"));
    }

    #[tokio::test]
    async fn report_findings_pass_through_without_verification() {
        let findings = vec![finding_report("f1")];
        let decisions = vec![decision("f1", FindingDisposition::Resolve)];
        let patch = ScriptedPatchVerifier::new_reject("would fail");
        let lint = ScriptedLintVerifier::new_reject("would fail");
        let out = run(
            &findings,
            decisions,
            &PathBuf::from("/tmp"),
            &patch,
            &lint,
            VerifyConfig::default(),
        )
        .await
        .unwrap();
        // Report findings skip verification; preserved as-is.
        assert_eq!(out[0].disposition, FindingDisposition::Resolve);
        assert_eq!(out[0].rationale, "from-triage");
    }

    // ── Markdownlint verifier (binary-shim tests) ─────────────────

    fn write_shim(dir: &Path, name: &str, exit_code: i32, stderr: &str) -> PathBuf {
        let shim = dir.join(name);
        let script = format!(
            "#!/bin/sh\n# ignore args\n>&2 printf '%s' '{}'\nexit {}\n",
            stderr.replace('\'', "'\\''"),
            exit_code
        );
        std::fs::write(&shim, script).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&shim).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&shim, perms).unwrap();
        }
        shim
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn markdownlint_accepts_when_shim_exits_zero() {
        let tmp = tempfile::tempdir().unwrap();
        let shim = write_shim(tmp.path(), "md-ok", 0, "");
        let v = MarkdownlintVerifier::with_binary(&shim);
        assert!(v.is_active());
        let outcome = v.verify("README.md", "# hello\n").await.unwrap();
        assert_eq!(outcome, VerifyOutcome::Accepted);
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn markdownlint_rejects_when_shim_exits_nonzero() {
        let tmp = tempfile::tempdir().unwrap();
        let shim = write_shim(tmp.path(), "md-fail", 1, "MD013 line too long");
        let v = MarkdownlintVerifier::with_binary(&shim);
        let outcome = v.verify("README.md", "line 1").await.unwrap();
        match outcome {
            VerifyOutcome::Rejected { reason } => {
                assert!(reason.contains("MD013"), "reason={reason}");
                assert!(reason.contains("README.md"));
            }
            VerifyOutcome::Accepted => panic!("expected Rejected"),
        }
    }

    #[tokio::test]
    async fn markdownlint_without_binary_falls_back_to_accept() {
        // Construct a verifier with no binary — simulates "markdownlint
        // not installed on this deploy." Should no-op accept.
        let v = MarkdownlintVerifier { binary: None };
        assert!(!v.is_active());
        let outcome = v.verify("README.md", "").await.unwrap();
        assert_eq!(outcome, VerifyOutcome::Accepted);
    }
}
