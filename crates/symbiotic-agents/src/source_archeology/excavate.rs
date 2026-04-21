//! Stage 1 — Excavate (deep repo inspection, deterministic, no LLM).
//!
//! See `docs/design/source-archeology.md` §Stage 1 — Excavate.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::Result;
use regex::Regex;
use serde::{Deserialize, Serialize};
use walkdir::WalkDir;

use super::archeology_types::ArcheologyTarget;

/// Maximum files walked per repo before the walk terminates.
pub const MAX_FILES_WALKED: usize = 5_000;

/// Maximum bytes read per file during the wire-mismatch scan.
pub const MAX_FILE_READ_BYTES: u64 = 500 * 1024;

// ── Types ──────────────────────────────────────────────────────────────

/// Typed output of Stage 1. Flat list of observations keyed by
/// `{category, path, evidence}`. Inputs to Stage 2 (Date) and Stage 3
/// (Diagnose).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExcavationReport {
    /// The `ArcheologyTarget.repo_id` this report covers.
    pub repo_id: String,
    /// Commit SHA at which Excavate observed the tree.
    pub observed_head: String,
    /// Flat list of observations. Stage 2 reads this directly.
    pub observations: Vec<Observation>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Observation {
    pub category: ObservationCategory,
    /// Repo-relative path the observation is about, or `""` for
    /// repo-wide observations (e.g. commit-pattern metrics).
    pub path: String,
    /// Free-form evidence string. Kept short so reports stay
    /// JSON-serializable without bloating.
    pub evidence: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservationCategory {
    /// Top-level signal file (`README*`, `LICENSE*`, `CLAUDE.md`,
    /// `AGENTS.md`, `CONTEXT.md`, `NAMING-CANON.md`, `docs/**`, `tasks/**`).
    SignalFile,
    /// Build system fingerprint.
    BuildSystem,
    /// CI configuration.
    CiConfig,
    /// Test-command wiring — docs referenced a script; does it exist?
    TestCommand,
    /// Git history summary (one per repo).
    CommitPattern,
    /// Contributor surface (one per repo).
    ContributorSurface,
    /// Dependency manifest present.
    Dependency,
    /// Wired-vs-declared mismatch (doc references a missing path).
    WireMismatch,
    /// Parser/IO failure on a file; recorded so the pipeline continues.
    ParseError,
}

// ── Entry point ────────────────────────────────────────────────────────

/// Run Stage 1 against the read-only clone at `clone_root`.
pub fn run(target: &ArcheologyTarget, clone_root: &Path) -> Result<ExcavationReport> {
    let mut observations: Vec<Observation> = Vec::new();
    let mut files_seen: usize = 0;
    let mut all_files: Vec<PathBuf> = Vec::new();

    // Pass 1+2: walk repo tree, recognize signal files, build the
    // full file list used by later passes.
    for entry in WalkDir::new(clone_root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| !is_git_dir(e.path()))
    {
        if files_seen >= MAX_FILES_WALKED {
            observations.push(Observation {
                category: ObservationCategory::ParseError,
                path: String::new(),
                evidence: format!(
                    "walk cap reached: {} files (limit {MAX_FILES_WALKED})",
                    files_seen
                ),
            });
            break;
        }

        let entry = match entry {
            Ok(e) => e,
            Err(err) => {
                observations.push(Observation {
                    category: ObservationCategory::ParseError,
                    path: err
                        .path()
                        .map(|p| relative_path(clone_root, p))
                        .unwrap_or_default(),
                    evidence: err.to_string(),
                });
                continue;
            }
        };

        let file_type = entry.file_type();
        // Regular files land in the signal-file sweep + later passes.
        // Symlinks still get appended to the walk list so a later pass
        // (wire-mismatch scan) can surface the dangling-target as a
        // ParseError observation rather than silently skipping.
        if !file_type.is_file() && !file_type.is_symlink() {
            continue;
        }
        files_seen += 1;

        let rel = relative_path(clone_root, entry.path());
        all_files.push(entry.path().to_path_buf());

        if file_type.is_file() && is_signal_file(&rel) {
            let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
            observations.push(Observation {
                category: ObservationCategory::SignalFile,
                path: rel.clone(),
                evidence: format!("size_bytes={size}"),
            });
        }
    }

    // Pass 3: build-system fingerprint.
    for manifest in [
        "Cargo.toml",
        "package.json",
        "pyproject.toml",
        "go.mod",
        "Makefile",
        "justfile",
    ] {
        if clone_root.join(manifest).exists() {
            observations.push(Observation {
                category: ObservationCategory::BuildSystem,
                path: manifest.to_string(),
                evidence: format!("manifest={manifest}"),
            });
        }
    }

    // Pass 4: CI configs.
    let gh_workflows = clone_root.join(".github").join("workflows");
    if gh_workflows.exists() {
        if let Ok(entries) = std::fs::read_dir(&gh_workflows) {
            for e in entries.flatten() {
                if e.file_type().map(|t| t.is_file()).unwrap_or(false) {
                    let rel = relative_path(clone_root, &e.path());
                    observations.push(Observation {
                        category: ObservationCategory::CiConfig,
                        path: rel,
                        evidence: "github_actions".to_string(),
                    });
                }
            }
        }
    }
    if clone_root.join(".gitlab-ci.yml").exists() {
        observations.push(Observation {
            category: ObservationCategory::CiConfig,
            path: ".gitlab-ci.yml".to_string(),
            evidence: "gitlab_ci".to_string(),
        });
    }
    let circle = clone_root.join(".circleci");
    if circle.exists() {
        observations.push(Observation {
            category: ObservationCategory::CiConfig,
            path: ".circleci".to_string(),
            evidence: "circleci".to_string(),
        });
    }

    // Pass 5: wire-mismatch scan across markdown files.
    let md_files: Vec<PathBuf> = all_files
        .iter()
        .filter(|p| {
            p.extension()
                .and_then(|e| e.to_str())
                .map(|e| e.eq_ignore_ascii_case("md"))
                .unwrap_or(false)
        })
        .cloned()
        .collect();
    scan_wire_mismatches(&md_files, clone_root, &mut observations);

    // Pass 6: commit pattern (one per repo).
    observations.extend(commit_pattern(clone_root));

    // Pass 7: contributor surface.
    let authors = distinct_authors(clone_root, 50);
    let has_license = all_files
        .iter()
        .any(|p| starts_with_license(&relative_path(clone_root, p)));
    observations.push(Observation {
        category: ObservationCategory::ContributorSurface,
        path: String::new(),
        evidence: format!("distinct_authors={authors}; has_license={has_license}"),
    });

    // Pass 8: dependency footprint (light; just counts).
    for (manifest, lockfile) in [
        ("Cargo.toml", "Cargo.lock"),
        ("package.json", "package-lock.json"),
        ("pyproject.toml", "poetry.lock"),
        ("go.mod", "go.sum"),
    ] {
        if clone_root.join(manifest).exists() {
            observations.push(Observation {
                category: ObservationCategory::Dependency,
                path: manifest.to_string(),
                evidence: format!("lockfile={}", clone_root.join(lockfile).exists()),
            });
        }
    }

    Ok(ExcavationReport {
        repo_id: target.repo_id.clone(),
        observed_head: head_sha(clone_root),
        observations,
    })
}

// ── Helpers ────────────────────────────────────────────────────────────

fn is_git_dir(p: &Path) -> bool {
    p.file_name()
        .and_then(|n| n.to_str())
        .map(|n| n == ".git")
        .unwrap_or(false)
}

fn relative_path(root: &Path, p: &Path) -> String {
    p.strip_prefix(root)
        .map(|r| r.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|_| p.to_string_lossy().to_string())
}

fn is_signal_file(rel: &str) -> bool {
    // Top-level signal names (case-insensitive prefix for README/LICENSE).
    let top_exact = ["CLAUDE.md", "AGENTS.md", "CONTEXT.md", "NAMING-CANON.md"];
    if top_exact.contains(&rel) {
        return true;
    }
    if let Some(base) = rel.rsplit('/').next() {
        let upper = base.to_ascii_uppercase();
        if upper.starts_with("README") || upper.starts_with("LICENSE") {
            return !rel.contains('/'); // only top-level README/LICENSE
        }
    }
    // docs/** and tasks/**
    rel.starts_with("docs/") || rel.starts_with("tasks/")
}

fn starts_with_license(rel: &str) -> bool {
    if rel.contains('/') {
        return false;
    }
    rel.to_ascii_uppercase().starts_with("LICENSE")
}

fn scan_wire_mismatches(
    md_files: &[PathBuf],
    clone_root: &Path,
    observations: &mut Vec<Observation>,
) {
    // Matches relative path references like `./scripts/foo.sh`, `../bin/x`,
    // backticked `./xxx` and bracketed links `[text](./a/b.md)`.
    let re = Regex::new(r"(?:\.{1,2}/[A-Za-z0-9._/-]+)").expect("static regex compiles");

    for path in md_files {
        let rel = relative_path(clone_root, path);

        let metadata = match std::fs::metadata(path) {
            Ok(m) => m,
            Err(err) => {
                observations.push(Observation {
                    category: ObservationCategory::ParseError,
                    path: rel.clone(),
                    evidence: format!("metadata: {err}"),
                });
                continue;
            }
        };
        if metadata.len() > MAX_FILE_READ_BYTES {
            observations.push(Observation {
                category: ObservationCategory::ParseError,
                path: rel.clone(),
                evidence: format!("file exceeds {MAX_FILE_READ_BYTES}B cap; truncating"),
            });
            continue;
        }

        let content = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(err) => {
                observations.push(Observation {
                    category: ObservationCategory::ParseError,
                    path: rel.clone(),
                    evidence: format!("read: {err}"),
                });
                continue;
            }
        };

        let doc_dir = path.parent().unwrap_or(clone_root);
        for cap in re.find_iter(&content) {
            let reference = cap.as_str();
            let target = doc_dir.join(reference);
            if !target.exists() {
                observations.push(Observation {
                    category: ObservationCategory::WireMismatch,
                    path: rel.clone(),
                    evidence: format!("missing={reference}"),
                });
            }
        }
    }
}

fn commit_pattern(clone_root: &Path) -> Vec<Observation> {
    let out = match Command::new("git")
        .arg("-C")
        .arg(clone_root)
        .args(["log", "--max-count=50", "--format=%H%x09%an%x09%at"])
        .output()
    {
        Ok(o) if o.status.success() => o,
        Ok(o) => {
            return vec![Observation {
                category: ObservationCategory::ParseError,
                path: String::new(),
                evidence: format!(
                    "git log exit={:?}: {}",
                    o.status.code(),
                    String::from_utf8_lossy(&o.stderr)
                ),
            }];
        }
        Err(err) => {
            return vec![Observation {
                category: ObservationCategory::ParseError,
                path: String::new(),
                evidence: format!("git log spawn: {err}"),
            }];
        }
    };
    let body = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<&str> = body.lines().collect();
    let count = lines.len();
    let mut authors = std::collections::HashSet::new();
    let mut timestamps: Vec<i64> = Vec::new();
    for line in &lines {
        let mut parts = line.split('\t');
        let _sha = parts.next();
        if let Some(author) = parts.next() {
            authors.insert(author.to_string());
        }
        if let Some(ts) = parts.next() {
            if let Ok(t) = ts.parse::<i64>() {
                timestamps.push(t);
            }
        }
    }
    let newest = timestamps.iter().copied().max().unwrap_or(0);
    let oldest = timestamps.iter().copied().min().unwrap_or(0);
    let now_secs = chrono::Utc::now().timestamp();
    let recent = (now_secs - newest) <= 14 * 24 * 60 * 60;
    vec![Observation {
        category: ObservationCategory::CommitPattern,
        path: String::new(),
        evidence: format!(
            "commits={count}; distinct_authors={}; spread_days={}; recent_14d={recent}",
            authors.len(),
            (newest - oldest) / (24 * 60 * 60)
        ),
    }]
}

fn distinct_authors(clone_root: &Path, max_count: usize) -> usize {
    let out = Command::new("git")
        .arg("-C")
        .arg(clone_root)
        .args(["log", &format!("--max-count={max_count}"), "--format=%an"])
        .output();
    match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
            .lines()
            .collect::<std::collections::HashSet<_>>()
            .len(),
        _ => 0,
    }
}

fn head_sha(clone_root: &Path) -> String {
    Command::new("git")
        .arg("-C")
        .arg(clone_root)
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .and_then(|o| {
            if o.status.success() {
                Some(String::from_utf8_lossy(&o.stdout).trim().to_string())
            } else {
                None
            }
        })
        .unwrap_or_default()
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source_archeology::archeology_types::{ArcheologyMode, ArcheologyTarget};
    use crate::source_archeology::fixtures::{build, Fixture};

    fn target() -> ArcheologyTarget {
        ArcheologyTarget {
            repo_id: "repo:flux".to_string(),
            base_branch: "main".to_string(),
            goal_id: "onboard".to_string(),
            allowed_paths: Vec::new(),
            mode: ArcheologyMode::default(),
        }
    }

    #[test]
    fn excavate_detects_readme_and_license() {
        let fx: Fixture = build(|_root, author| {
            author.commit_file("README.md", "# Flux\n", 10, "alice@example.com")?;
            author.commit_file("LICENSE", "MIT\n", 10, "alice@example.com")?;
            Ok(())
        })
        .expect("build fixture");

        let report = run(&target(), &fx.clone_root()).expect("excavate");
        let signal_paths: Vec<_> = report
            .observations
            .iter()
            .filter(|o| o.category == ObservationCategory::SignalFile)
            .map(|o| o.path.as_str())
            .collect();
        assert!(
            signal_paths.contains(&"README.md"),
            "README.md missing in {signal_paths:?}"
        );
        assert!(
            signal_paths.contains(&"LICENSE"),
            "LICENSE missing in {signal_paths:?}"
        );
        assert!(!report.observed_head.is_empty());
    }

    #[test]
    fn excavate_detects_wire_mismatch() {
        let fx = build(|_root, author| {
            author.commit_file(
                "README.md",
                "Run `./scripts/build.sh` to build.\n",
                10,
                "alice@example.com",
            )?;
            Ok(())
        })
        .expect("build fixture");

        let report = run(&target(), &fx.clone_root()).expect("excavate");
        let mismatches: Vec<_> = report
            .observations
            .iter()
            .filter(|o| o.category == ObservationCategory::WireMismatch)
            .collect();
        assert_eq!(
            mismatches.len(),
            1,
            "expected 1 wire-mismatch, got {:?}",
            mismatches
        );
        assert_eq!(mismatches[0].path, "README.md");
        assert!(mismatches[0].evidence.contains("./scripts/build.sh"));
    }

    #[test]
    #[cfg(unix)]
    fn excavate_parse_error_is_non_fatal() {
        let fx = build(|_root, author| {
            author.commit_file("README.md", "# Flux\n", 10, "alice@example.com")?;
            // Don't commit the symlink — git doesn't follow broken symlinks
            // to stage them; we just need the file to be present on disk
            // during the walk.
            author.create_dangling_symlink("docs/broken.md")?;
            Ok(())
        })
        .expect("build fixture");

        let report = run(&target(), &fx.clone_root()).expect("excavate");
        // README should still be observed.
        assert!(report
            .observations
            .iter()
            .any(|o| o.category == ObservationCategory::SignalFile && o.path == "README.md"));
        // At least one parse-error observation should surface for the
        // dangling symlink (either metadata or read error).
        assert!(
            report
                .observations
                .iter()
                .any(|o| o.category == ObservationCategory::ParseError),
            "expected ParseError observation"
        );
    }

    #[test]
    fn excavate_caps_file_count() {
        let fx = build(|root, author| {
            // Seed one committed file so HEAD exists.
            author.commit_file("README.md", "# Flux\n", 10, "alice@example.com")?;
            // Create MAX_FILES_WALKED + 50 uncommitted scratch files on disk.
            // The walk caps files_seen at MAX_FILES_WALKED.
            let scratch_dir = root.join("scratch");
            std::fs::create_dir_all(&scratch_dir).unwrap();
            for i in 0..(MAX_FILES_WALKED + 50) {
                std::fs::write(scratch_dir.join(format!("f{i}.txt")), "x").unwrap();
            }
            Ok(())
        })
        .expect("build fixture");

        let report = run(&target(), &fx.clone_root()).expect("excavate");
        let cap_hits: Vec<_> = report
            .observations
            .iter()
            .filter(|o| {
                o.category == ObservationCategory::ParseError
                    && o.evidence.contains("walk cap reached")
            })
            .collect();
        assert_eq!(
            cap_hits.len(),
            1,
            "expected exactly one walk-cap observation, got {}",
            cap_hits.len()
        );
    }

    #[test]
    fn excavation_report_serde_roundtrip() {
        let report = ExcavationReport {
            repo_id: "repo:flux".to_string(),
            observed_head: "deadbeef".to_string(),
            observations: vec![
                Observation {
                    category: ObservationCategory::SignalFile,
                    path: "README.md".to_string(),
                    evidence: "size_bytes=42".to_string(),
                },
                Observation {
                    category: ObservationCategory::WireMismatch,
                    path: "README.md".to_string(),
                    evidence: "missing=./bin/x".to_string(),
                },
                Observation {
                    category: ObservationCategory::ParseError,
                    path: "broken.md".to_string(),
                    evidence: "read: ENOENT".to_string(),
                },
            ],
        };
        let json = serde_json::to_string(&report).unwrap();
        let parsed: ExcavationReport = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.repo_id, report.repo_id);
        assert_eq!(parsed.observations.len(), 3);
        assert_eq!(
            parsed.observations[0].category,
            ObservationCategory::SignalFile
        );
        assert!(
            json.contains("\"signal_file\""),
            "snake_case expected: {json}"
        );
    }
}
