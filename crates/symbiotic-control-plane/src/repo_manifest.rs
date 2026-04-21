//! Repo manifest types and validation for durable project-attached repos.
//!
//! A `RepoManifest` is an Archive-native record parsed from
//! `operations/projects/{project}/repos/{slug}.md`. It describes a single
//! external source repository attached to a project, its mirror policy,
//! credential binding, agent-capability scopes, hooks, and (for non-`Source`
//! roles) its indexing policy for the Attached Knowledge Base (T129).
//!
//! See `docs/design/repo-manifest.md` for the full specification.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use symbiotic_agents::{FindingSeverity, StageConfigs};
use symbiotic_trust::AgentTrustLevel;
use thiserror::Error;

// ── Top-level Manifest ─────────────────────────────────────────────────

/// A repo manifest parsed from
/// `operations/projects/{project}/repos/{slug}.md`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoManifest {
    pub id: String,
    pub project_id: String,
    pub slug: String,
    pub title: String,
    pub state: RepoState,
    #[serde(default = "RepoRole::default_source")]
    pub repo_role: RepoRole,

    pub source: RepoSource,
    pub credential: RepoCredentialBinding,
    pub mirror: RepoMirrorPolicy,
    pub checkout: RepoCheckoutPolicy,
    pub agent_scopes: RepoAgentScopes,
    pub hooks: RepoHooks,

    /// Present iff `repo_role != Source`. Drives the T129 AKB pipeline.
    #[serde(default)]
    pub indexing: Option<RepoIndexingPolicy>,

    /// Optional per-repo overrides for Source Archeology tunables (T128).
    /// Unset fields fall through to `StageConfigs::default()` — the
    /// zero-config path. Per `docs/design/agent-tunables.md`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archeology_policy: Option<ArcheologyPolicy>,

    pub metadata: RepoMetadata,

    /// Human-readable Markdown body (not part of YAML frontmatter).
    #[serde(skip)]
    pub body_markdown: String,
}

// ── Enums ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepoState {
    Active,
    Paused,
    Detached,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepoRole {
    Source,
    DocsAkb,
    ReferenceLibrary,
}

impl RepoRole {
    /// Default repo role when the frontmatter omits `repo_role`.
    pub fn default_source() -> Self {
        RepoRole::Source
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepoProvider {
    Github,
    Gitlab,
    Gitea,
    Local,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialScope {
    Read,
    Push,
    Admin,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MirrorDirection {
    PullOnly,
    PushOnly,
    Bidirectional,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AkbTier {
    Raw,
    Distilled,
}

// ── Nested Structs ─────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoSource {
    pub url: String,
    pub provider: RepoProvider,
    pub default_branch: String,
    #[serde(default)]
    pub protected_branches: Vec<String>,
    #[serde(default)]
    pub pinned_head: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoCredentialBinding {
    pub id: String,
    pub scope: CredentialScope,
    pub trust_floor: AgentTrustLevel,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoMirrorPolicy {
    pub internal_bare_path: PathBuf,
    pub direction: MirrorDirection,
    pub sync_interval_secs: u64,
    #[serde(default)]
    pub last_pulled_at: Option<String>,
    #[serde(default)]
    pub last_pushed_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoCheckoutPolicy {
    pub worktree_root: PathBuf,
    pub agent_branch_prefix: String,
    pub max_concurrent_worktrees: u16,
    pub cleanup_on_goal_close: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoAgentScopes {
    pub read: Vec<String>,
    pub write: Vec<String>,
    pub push_external: bool,
    #[serde(default)]
    pub requires_operator_approval_for: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoHooks {
    #[serde(default)]
    pub on_attach: Option<String>,
    #[serde(default)]
    pub on_drift_detected: Option<String>,
    #[serde(default)]
    pub on_detach: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoIndexingPolicy {
    pub root_paths: Vec<String>,
    #[serde(default)]
    pub exclude_patterns: Vec<String>,
    pub distillery_config: RepoDistilleryConfig,
    pub tier_policy: RepoTierPolicy,
    pub refresh: RepoRefreshPolicy,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoDistilleryConfig {
    pub enable_reweave: bool,
    pub enable_semantic_verify: bool,
    pub model: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoTierPolicy {
    pub default_tier: AkbTier,
    pub auto_promote: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoRefreshPolicy {
    pub on_commit: bool,
    pub interval_secs: u64,
    pub incremental: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoMetadata {
    pub attached_at: String,
    pub attached_by: String,
    #[serde(default)]
    pub notes: String,
}

// ── Source Archeology policy overrides (T128) ──────────────────────────

/// Per-repo overrides for Source Archeology stage tunables. Every field
/// is optional; unset → fall through to `StageConfigs::default()`. Per
/// `docs/design/agent-tunables.md`: conservative defaults, operators
/// override only what they need.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ArcheologyPolicy {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diagnose_confidence_floor: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scaffold_max_source_side_findings: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handoff_max_questions: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handoff_max_report_chars: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub triage_out_of_scope_defer_max_severity: Option<FindingSeverity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub triage_escalate_on_peer_disagreement: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verify_lint_scaffold_files: Option<bool>,
    /// Auto-approve archeology pushes if the finding's severity is ≤ this
    /// threshold. `None` (default) = every push needs operator approval.
    /// Consumed by T128 §15 `archeology_dispatch`. Severity ordering:
    /// `Low < Medium < High < Critical` (explicit `Ord` impl on
    /// `FindingSeverity`); e.g. `Some(Medium)` auto-approves Low + Medium.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_approve_max_severity: Option<FindingSeverity>,
    /// Whether to post a per-run summary message after every archeology
    /// run. `None` defaults to `false` (quiet on routine runs). Consumed
    /// by T128 §16 `archeology_notify`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notify_post_run_summary: Option<bool>,
    /// Max number of Escalate findings posted as individual messages
    /// before collapsing the rest into "…and N more." `None` defaults
    /// to 5. Consumed by T128 §16 `archeology_notify`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notify_max_escalate_messages: Option<usize>,
}

impl ArcheologyPolicy {
    /// Overlay this policy onto a `StageConfigs`. Fields set on the
    /// policy override the defaults in `configs`; unset fields leave
    /// the existing value untouched.
    pub fn apply_to(&self, configs: &mut StageConfigs) {
        if let Some(v) = self.diagnose_confidence_floor {
            configs.diagnose.confidence_floor = v;
        }
        if let Some(v) = self.scaffold_max_source_side_findings {
            configs.scaffold.max_source_side_findings = v;
        }
        if let Some(v) = self.handoff_max_questions {
            configs.handoff.max_questions = v;
        }
        if let Some(v) = self.handoff_max_report_chars {
            configs.handoff.max_report_chars = v;
        }
        if let Some(v) = self.triage_out_of_scope_defer_max_severity {
            configs.triage.out_of_scope_defer_max_severity = v;
        }
        if let Some(v) = self.triage_escalate_on_peer_disagreement {
            configs.triage.escalate_on_peer_disagreement = v;
        }
        if let Some(v) = self.verify_lint_scaffold_files {
            configs.verify.lint_scaffold_files = v;
        }
    }

    /// Convenience: start from `StageConfigs::default()` and apply this
    /// policy on top. Useful when the caller doesn't already have a
    /// pre-populated `StageConfigs`.
    pub fn into_stage_configs(&self) -> StageConfigs {
        let mut configs = StageConfigs::default();
        self.apply_to(&mut configs);
        configs
    }
}

// ── Errors ─────────────────────────────────────────────────────────────

/// Load-time schema errors surfaced by `RepoManifest::validate`.
#[derive(Debug, Error)]
pub enum RepoManifestError {
    #[error("repo_role: source must not carry an `indexing` block")]
    IndexingOnSource,
    #[error("repo_role: {role:?} requires an `indexing` block")]
    MissingIndexing { role: RepoRole },
    #[error("id `{id}` suffix does not match slug `{slug}`")]
    MismatchedIdSlug { id: String, slug: String },
    #[error("id `{id}` must begin with `repo:`")]
    InvalidIdPrefix { id: String },
    #[error("project_id `{project_id}` must begin with `project:`")]
    InvalidProjectIdPrefix { project_id: String },
    #[error("required field `{field}` must not be empty")]
    EmptyField { field: &'static str },
}

// ── Validation ─────────────────────────────────────────────────────────

impl RepoManifest {
    /// Load-time schema check. Enforces role ↔ indexing coupling, id/slug
    /// consistency, and required-field presence. `push_external` without a
    /// populated approval list is logged but NOT an error.
    pub fn validate(&self) -> Result<(), RepoManifestError> {
        // 1. `id` must be `repo:{slug}`.
        let id_suffix =
            self.id
                .strip_prefix("repo:")
                .ok_or_else(|| RepoManifestError::InvalidIdPrefix {
                    id: self.id.clone(),
                })?;
        if id_suffix != self.slug {
            return Err(RepoManifestError::MismatchedIdSlug {
                id: self.id.clone(),
                slug: self.slug.clone(),
            });
        }

        // 2. `project_id` prefix.
        if !self.project_id.starts_with("project:") {
            return Err(RepoManifestError::InvalidProjectIdPrefix {
                project_id: self.project_id.clone(),
            });
        }

        // 3. Role ↔ indexing coupling.
        match self.repo_role {
            RepoRole::Source => {
                if self.indexing.is_some() {
                    return Err(RepoManifestError::IndexingOnSource);
                }
            }
            role @ (RepoRole::DocsAkb | RepoRole::ReferenceLibrary) => {
                if self.indexing.is_none() {
                    return Err(RepoManifestError::MissingIndexing { role });
                }
            }
        }

        // 4. push_external + empty approval list → warn, don't fail.
        if self.agent_scopes.push_external
            && self.agent_scopes.requires_operator_approval_for.is_empty()
        {
            tracing::warn!(
                repo_id = %self.id,
                "repo manifest has push_external=true with empty requires_operator_approval_for list; \
                 this is supported but loud (see docs/design/repo-manifest.md §Security Notes)"
            );
        }

        // 5. Required-field non-emptiness.
        if self.source.url.is_empty() {
            return Err(RepoManifestError::EmptyField {
                field: "source.url",
            });
        }
        if self.source.default_branch.is_empty() {
            return Err(RepoManifestError::EmptyField {
                field: "source.default_branch",
            });
        }
        if self.credential.id.is_empty() {
            return Err(RepoManifestError::EmptyField {
                field: "credential.id",
            });
        }

        Ok(())
    }
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::ManifestParser;
    use std::path::Path;
    use tempfile::TempDir;

    fn write_file(dir: &Path, rel_path: &str, content: &str) {
        let path = dir.join(rel_path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, content).unwrap();
    }

    /// Minimal `source`-role YAML without an `indexing` block.
    fn source_yaml(id: &str, slug: &str, project_id: &str) -> String {
        format!(
            r#"---
id: "{id}"
project_id: "{project_id}"
slug: {slug}
title: "{slug}"
state: active
repo_role: source

source:
  url: "git@github.com:example/{slug}.git"
  provider: github
  default_branch: "main"
  protected_branches: ["main"]

credential:
  id: "credential:example-{slug}-push"
  scope: push
  trust_floor: CredentialAccess

mirror:
  internal_bare_path: "data/git-server/repos/{slug}.git"
  direction: bidirectional
  sync_interval_secs: 300

checkout:
  worktree_root: "data/worktrees/{slug}/"
  agent_branch_prefix: "agent/"
  max_concurrent_worktrees: 4
  cleanup_on_goal_close: true

agent_scopes:
  read: ["archive.read", "file.read"]
  write: ["archive.read", "archive.write", "file.read", "file.write"]
  push_external: false
  requires_operator_approval_for: ["push_external"]

hooks: {{}}

metadata:
  attached_at: "2026-04-16T00:00:00Z"
  attached_by: "operator"
  notes: "test repo"
---

# {slug}

Test body.
"#
        )
    }

    /// `docs_akb`-role YAML with an `indexing` block.
    fn docs_akb_yaml(id: &str, slug: &str, project_id: &str) -> String {
        format!(
            r#"---
id: "{id}"
project_id: "{project_id}"
slug: {slug}
title: "{slug}"
state: active
repo_role: docs_akb

source:
  url: "git@github.com:example/{slug}.git"
  provider: github
  default_branch: "main"

credential:
  id: "credential:example-{slug}-read"
  scope: read
  trust_floor: ReadOnly

mirror:
  internal_bare_path: "data/git-server/repos/{slug}.git"
  direction: pull_only
  sync_interval_secs: 3600

checkout:
  worktree_root: "data/worktrees/{slug}/"
  agent_branch_prefix: "agent/"
  max_concurrent_worktrees: 1
  cleanup_on_goal_close: true

agent_scopes:
  read: ["akb.query"]
  write: []
  push_external: false

hooks: {{}}

indexing:
  root_paths: ["docs/", "README.md"]
  exclude_patterns: ["**/node_modules/**"]
  distillery_config:
    enable_reweave: false
    enable_semantic_verify: true
    model: "qwen3.5"
  tier_policy:
    default_tier: distilled
    auto_promote: false
  refresh:
    on_commit: true
    interval_secs: 3600
    incremental: true

metadata:
  attached_at: "2026-04-16T00:00:00Z"
  attached_by: "operator"
  notes: "akb repo"
---

# {slug}

Docs body.
"#
        )
    }

    fn parse_yaml(yaml_with_body: &str) -> RepoManifest {
        // Split frontmatter manually (mirrors `split_frontmatter` behavior).
        let trimmed = yaml_with_body.trim_start();
        let after_open = trimmed.strip_prefix("---\n").unwrap();
        let closing_pos = after_open.find("\n---").unwrap();
        let yaml_str = &after_open[..closing_pos];
        let rest = &after_open[closing_pos + 4..];
        let body = rest.strip_prefix('\n').unwrap_or(rest);
        let mut manifest: RepoManifest = serde_yml::from_str(yaml_str).unwrap();
        manifest.body_markdown = body.to_string();
        manifest
    }

    #[test]
    fn parse_source_repo_minimum() {
        let yaml = source_yaml("repo:flux", "flux", "project:flux");
        let manifest = parse_yaml(&yaml);
        assert_eq!(manifest.slug, "flux");
        assert_eq!(manifest.repo_role, RepoRole::Source);
        assert_eq!(manifest.state, RepoState::Active);
        assert!(manifest.indexing.is_none());
        assert!(manifest.body_markdown.contains("# flux"));
        manifest.validate().unwrap();
    }

    #[test]
    fn parse_docs_akb_repo_with_indexing() {
        let yaml = docs_akb_yaml("repo:flux-docs", "flux-docs", "project:flux");
        let manifest = parse_yaml(&yaml);
        assert_eq!(manifest.repo_role, RepoRole::DocsAkb);
        let indexing = manifest.indexing.as_ref().expect("indexing present");
        assert_eq!(indexing.root_paths, vec!["docs/", "README.md"]);
        assert_eq!(indexing.tier_policy.default_tier, AkbTier::Distilled);
        manifest.validate().unwrap();
    }

    #[test]
    fn reject_source_with_indexing() {
        // Take a docs_akb yaml and flip role to source while keeping `indexing`.
        let yaml = docs_akb_yaml("repo:flux", "flux", "project:flux")
            .replace("repo_role: docs_akb", "repo_role: source");
        let manifest = parse_yaml(&yaml);
        let err = manifest.validate().unwrap_err();
        assert!(matches!(err, RepoManifestError::IndexingOnSource));
    }

    #[test]
    fn reject_docs_akb_without_indexing() {
        // Take a source yaml and flip role to docs_akb without adding `indexing`.
        let yaml = source_yaml("repo:flux-docs", "flux-docs", "project:flux")
            .replace("repo_role: source", "repo_role: docs_akb");
        let manifest = parse_yaml(&yaml);
        let err = manifest.validate().unwrap_err();
        assert!(matches!(
            err,
            RepoManifestError::MissingIndexing {
                role: RepoRole::DocsAkb
            }
        ));
    }

    #[test]
    fn reject_reference_library_without_indexing() {
        let yaml = source_yaml("repo:flux-ref", "flux-ref", "project:flux")
            .replace("repo_role: source", "repo_role: reference_library");
        let manifest = parse_yaml(&yaml);
        let err = manifest.validate().unwrap_err();
        assert!(matches!(
            err,
            RepoManifestError::MissingIndexing {
                role: RepoRole::ReferenceLibrary
            }
        ));
    }

    #[test]
    fn reject_mismatched_id_slug() {
        // id suffix = "foo", slug = "bar" — mismatch.
        let yaml = source_yaml("repo:foo", "bar", "project:flux");
        let manifest = parse_yaml(&yaml);
        let err = manifest.validate().unwrap_err();
        assert!(matches!(err, RepoManifestError::MismatchedIdSlug { .. }));
    }

    #[test]
    fn push_external_without_approval_list_warns_not_fails() {
        let yaml = source_yaml("repo:flux", "flux", "project:flux")
            .replace("push_external: false", "push_external: true")
            .replace(
                "requires_operator_approval_for: [\"push_external\"]",
                "requires_operator_approval_for: []",
            );
        let manifest = parse_yaml(&yaml);
        // Passes validation; the caller may observe a `tracing::warn!`.
        manifest.validate().unwrap();
    }

    #[test]
    fn parse_project_repos_fans_out() {
        let tmp = TempDir::new().unwrap();
        let project_rel = "operations/projects/flux";

        write_file(
            tmp.path(),
            &format!("{project_rel}/project.md"),
            r#"---
id: "project:flux"
slug: flux
title: "Flux"
state: active
repos: ["flux"]
---

# Flux
"#,
        );

        write_file(
            tmp.path(),
            &format!("{project_rel}/repos/flux.md"),
            &source_yaml("repo:flux", "flux", "project:flux"),
        );

        let parser = ManifestParser::new();
        let state = parser.parse_all(tmp.path()).unwrap();
        assert_eq!(state.projects.len(), 1);
        let project = &state.projects[0];
        assert_eq!(project.resolved_repos.len(), 1);
        assert_eq!(project.resolved_repos[0].slug, "flux");
        assert_eq!(project.resolved_repos[0].repo_role, RepoRole::Source);
    }

    #[test]
    fn parse_project_repos_skips_malformed() {
        let tmp = TempDir::new().unwrap();
        let project_rel = "operations/projects/flux";

        write_file(
            tmp.path(),
            &format!("{project_rel}/project.md"),
            r#"---
id: "project:flux"
slug: flux
title: "Flux"
state: active
repos: ["flux", "broken"]
---

# Flux
"#,
        );

        // Good repo.
        write_file(
            tmp.path(),
            &format!("{project_rel}/repos/flux.md"),
            &source_yaml("repo:flux", "flux", "project:flux"),
        );

        // Malformed (id/slug mismatch → validate() fails).
        write_file(
            tmp.path(),
            &format!("{project_rel}/repos/broken.md"),
            &source_yaml("repo:totally-wrong", "broken", "project:flux"),
        );

        let parser = ManifestParser::new();
        let state = parser.parse_all(tmp.path()).unwrap();
        assert_eq!(state.projects.len(), 1);
        let project = &state.projects[0];
        assert_eq!(project.resolved_repos.len(), 1);
        assert_eq!(project.resolved_repos[0].slug, "flux");
    }

    // ── ArcheologyPolicy ──────────────────────────────────────────

    #[test]
    fn archeology_policy_default_is_all_none() {
        let p = ArcheologyPolicy::default();
        assert!(p.diagnose_confidence_floor.is_none());
        assert!(p.scaffold_max_source_side_findings.is_none());
        assert!(p.handoff_max_questions.is_none());
        assert!(p.triage_escalate_on_peer_disagreement.is_none());
    }

    #[test]
    fn archeology_policy_partial_yaml_roundtrip() {
        let yaml = "diagnose_confidence_floor: 0.65\nhandoff_max_questions: 3\n";
        let p: ArcheologyPolicy = serde_yml::from_str(yaml).unwrap();
        assert_eq!(p.diagnose_confidence_floor, Some(0.65));
        assert_eq!(p.handoff_max_questions, Some(3));
        assert!(p.scaffold_max_source_side_findings.is_none());
    }

    #[test]
    fn archeology_policy_skip_serializing_none_fields() {
        let p = ArcheologyPolicy {
            diagnose_confidence_floor: Some(0.85),
            ..Default::default()
        };
        let yaml = serde_yml::to_string(&p).unwrap();
        // Only the populated field should appear.
        assert!(yaml.contains("diagnose_confidence_floor"));
        assert!(!yaml.contains("handoff_max_questions"));
        assert!(!yaml.contains("verify_lint_scaffold_files"));
    }

    #[test]
    fn apply_to_overrides_only_set_fields() {
        let mut configs = StageConfigs::default();
        let original_diagnose_floor = configs.diagnose.confidence_floor;
        let original_handoff_max_q = configs.handoff.max_questions;

        let policy = ArcheologyPolicy {
            diagnose_confidence_floor: Some(0.65),
            // Leave handoff_max_questions as None → unchanged.
            ..Default::default()
        };
        policy.apply_to(&mut configs);

        assert!((configs.diagnose.confidence_floor - 0.65).abs() < f32::EPSILON);
        assert_eq!(
            configs.handoff.max_questions, original_handoff_max_q,
            "unset policy fields preserve defaults"
        );
        assert_ne!(
            configs.diagnose.confidence_floor, original_diagnose_floor,
            "set policy fields override defaults"
        );
    }

    #[test]
    fn into_stage_configs_is_defaults_plus_overrides() {
        let policy = ArcheologyPolicy {
            scaffold_max_source_side_findings: Some(50),
            triage_escalate_on_peer_disagreement: Some(false),
            ..Default::default()
        };
        let configs = policy.into_stage_configs();
        assert_eq!(configs.scaffold.max_source_side_findings, 50);
        assert!(!configs.triage.escalate_on_peer_disagreement);
        // Untouched defaults preserved.
        assert!((configs.diagnose.confidence_floor - 0.80).abs() < f32::EPSILON);
    }

    #[test]
    fn triage_severity_override_accepts_snake_case_yaml() {
        let yaml = "triage_out_of_scope_defer_max_severity: high\n";
        let p: ArcheologyPolicy = serde_yml::from_str(yaml).unwrap();
        assert_eq!(
            p.triage_out_of_scope_defer_max_severity,
            Some(FindingSeverity::High)
        );
    }

    #[test]
    fn archeology_policy_new_fields_default_none() {
        let p = ArcheologyPolicy::default();
        assert!(p.auto_approve_max_severity.is_none());
        assert!(p.notify_post_run_summary.is_none());
        assert!(p.notify_max_escalate_messages.is_none());
    }

    #[test]
    fn archeology_policy_new_fields_yaml_roundtrip() {
        let yaml = "auto_approve_max_severity: low\nnotify_post_run_summary: true\nnotify_max_escalate_messages: 3\n";
        let p: ArcheologyPolicy = serde_yml::from_str(yaml).unwrap();
        assert_eq!(p.auto_approve_max_severity, Some(FindingSeverity::Low));
        assert_eq!(p.notify_post_run_summary, Some(true));
        assert_eq!(p.notify_max_escalate_messages, Some(3));
    }
}
