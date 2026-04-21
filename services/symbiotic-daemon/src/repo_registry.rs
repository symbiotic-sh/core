//! In-memory index of `RepoManifest`s, loaded from the Archive at startup and
//! mutated afterwards via event-driven handlers.
//!
//! The registry exposes typed hooks — `on_repo_attached`, `on_repo_state_changed`,
//! `on_repo_detached` — that mirror the canonical lifecycle events emitted when
//! repo manifests are attached, paused, resumed, or detached. Per
//! `docs/design/repo-manifest.md` §Module Layout, reload is event-driven rather
//! than FS-watched: the Archive writer is the single source of truth, and the
//! registry trusts the event stream to keep its view current.
//!
//! This module is data-only for T126 §03 — no mirror loop, no `AccessBroker`
//! wiring, no lifecycle event emission. Those land in §04/§05/§06.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use symbiotic_control_plane::{ManifestParser, RepoManifest, RepoState};
use thiserror::Error;
use tokio::sync::Mutex;

/// In-memory index of repo manifests keyed by their canonical id (`repo:{slug}`).
///
/// Detached manifests remain in the map with `state == Detached` for audit,
/// per the design-doc security note: *"A detached repo retains its manifest
/// for audit"*.
#[derive(Debug, Default)]
pub struct RepoRegistry {
    /// All repo manifests ever attached, keyed by `RepoManifest::id`.
    /// Detached manifests remain in the map with `state == Detached`.
    by_id: HashMap<String, RepoManifest>,
}

#[derive(Debug, Error)]
pub enum RepoRegistryError {
    #[error("unknown repo id: {0}")]
    UnknownId(String),
    #[error("duplicate attach for repo id: {0}")]
    DuplicateAttach(String),
    #[error("invalid state transition for {id}: {from:?} -> {to:?}")]
    InvalidTransition {
        id: String,
        from: RepoState,
        to: RepoState,
    },
}

/// Shared handle for the registry when it needs to be mutated from multiple
/// async tasks. Wraps the registry in a `tokio::sync::Mutex` so state-transition
/// checks stay serialized.
pub type SharedRepoRegistry = Arc<Mutex<RepoRegistry>>;

impl RepoRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Load every repo manifest from the Archive into a fresh registry.
    ///
    /// Delegates to [`ManifestParser::parse_all`] and flattens
    /// `ProjectManifest::resolved_repos` across all projects into one map.
    ///
    /// Any duplicate id across projects is a hard error
    /// (`RepoRegistryError::DuplicateAttach`), matching the design doc's
    /// "manifest copied per project" rule — cross-project reuse is explicitly
    /// rejected.
    pub fn load_from_archive(kb_path: &Path) -> Result<Self> {
        let state = ManifestParser::new()
            .parse_all(kb_path)
            .with_context(|| format!("parsing archive at {}", kb_path.display()))?;

        let mut registry = RepoRegistry::new();
        for manifest in state
            .projects
            .iter()
            .flat_map(|p| p.resolved_repos.iter().cloned())
        {
            let id = manifest.id.clone();
            if registry.by_id.contains_key(&id) {
                return Err(anyhow::Error::new(RepoRegistryError::DuplicateAttach(id)));
            }
            registry.by_id.insert(id, manifest);
        }
        Ok(registry)
    }

    /// Look up a manifest by its canonical id (`repo:{slug}`).
    pub fn get(&self, id: &str) -> Option<&RepoManifest> {
        self.by_id.get(id)
    }

    /// Return every manifest attached to the given project id, regardless of state.
    pub fn list_by_project(&self, project_id: &str) -> Vec<&RepoManifest> {
        self.by_id
            .values()
            .filter(|m| m.project_id == project_id)
            .collect()
    }

    /// Return every manifest currently in the `Active` state.
    pub fn list_active(&self) -> Vec<&RepoManifest> {
        self.by_id
            .values()
            .filter(|m| m.state == RepoState::Active)
            .collect()
    }

    /// Return every manifest in the registry, regardless of state. Useful for
    /// audit tooling that needs to see detached entries too.
    pub fn list_all(&self) -> Vec<&RepoManifest> {
        self.by_id.values().collect()
    }

    /// Insert a newly attached manifest. Errors if the id already exists.
    pub fn on_repo_attached(&mut self, manifest: RepoManifest) -> Result<(), RepoRegistryError> {
        if self.by_id.contains_key(&manifest.id) {
            return Err(RepoRegistryError::DuplicateAttach(manifest.id.clone()));
        }
        self.by_id.insert(manifest.id.clone(), manifest);
        Ok(())
    }

    /// Mutate the `state` field of an existing manifest, enforcing the
    /// valid-transition rules from `docs/design/repo-manifest.md`:
    ///
    /// - `Active ↔ Paused` allowed in both directions.
    /// - `Active → Detached` and `Paused → Detached` allowed.
    /// - `Detached → *` rejected — detached is terminal per the security note
    ///   *"A detached repo retains its manifest for audit but its
    ///   `internal_bare_path` should be archived to a read-only location to
    ///   prevent accidental reuse of stale credentials"*.
    /// - Same-state transitions are a no-op and return `Ok(())`.
    pub fn on_repo_state_changed(
        &mut self,
        id: &str,
        new_state: RepoState,
    ) -> Result<(), RepoRegistryError> {
        let manifest = self
            .by_id
            .get_mut(id)
            .ok_or_else(|| RepoRegistryError::UnknownId(id.to_string()))?;
        let current = manifest.state;
        if current == new_state {
            return Ok(());
        }
        let valid = match (current, new_state) {
            (RepoState::Active, RepoState::Paused)
            | (RepoState::Paused, RepoState::Active)
            | (RepoState::Active, RepoState::Detached)
            | (RepoState::Paused, RepoState::Detached) => true,
            // Detached is terminal.
            (RepoState::Detached, _) => false,
            // All same-state transitions were already handled above.
            _ => false,
        };
        if !valid {
            return Err(RepoRegistryError::InvalidTransition {
                id: id.to_string(),
                from: current,
                to: new_state,
            });
        }
        manifest.state = new_state;
        Ok(())
    }

    /// Convenience wrapper that sets `state = Detached` via the transition
    /// check. Detached manifests are kept in the registry for audit.
    pub fn on_repo_detached(&mut self, id: &str) -> Result<(), RepoRegistryError> {
        self.on_repo_state_changed(id, RepoState::Detached)
    }

    /// Number of manifests in the registry (including detached ones).
    pub fn len(&self) -> usize {
        self.by_id.len()
    }

    /// Whether the registry is empty. Convenience complement to [`len`].
    pub fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use symbiotic_control_plane::{RepoManifest, RepoRole, RepoState};
    use tempfile::TempDir;

    fn write_file(dir: &Path, rel_path: &str, content: &str) {
        let path = dir.join(rel_path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, content).unwrap();
    }

    /// Minimal `source`-role YAML (copied from `repo_manifest.rs` tests).
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

    /// Minimal `ProjectManifest` markdown.
    fn project_md(project_id: &str, slug: &str, repos: &[&str]) -> String {
        let repos_yaml = repos
            .iter()
            .map(|r| format!("\"{r}\""))
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            r#"---
id: "{project_id}"
slug: {slug}
title: "{slug}"
state: active
repos: [{repos_yaml}]
---

# {slug}
"#
        )
    }

    /// Build a valid in-memory `RepoManifest` for non-archive tests.
    /// Uses the YAML fixture + the same split-frontmatter trick as
    /// `repo_manifest::tests::parse_yaml`.
    fn build_manifest(id: &str, slug: &str, project_id: &str) -> RepoManifest {
        let yaml = source_yaml(id, slug, project_id);
        let trimmed = yaml.trim_start();
        let after_open = trimmed.strip_prefix("---\n").unwrap();
        let closing_pos = after_open.find("\n---").unwrap();
        let yaml_str = &after_open[..closing_pos];
        let rest = &after_open[closing_pos + 4..];
        let body = rest.strip_prefix('\n').unwrap_or(rest);
        let mut manifest: RepoManifest = serde_yml::from_str(yaml_str).unwrap();
        manifest.body_markdown = body.to_string();
        manifest
    }

    // 1
    #[test]
    fn new_registry_is_empty() {
        let registry = RepoRegistry::new();
        assert_eq!(registry.len(), 0);
        assert!(registry.is_empty());
        assert!(registry.list_all().is_empty());
        assert!(registry.list_active().is_empty());
    }

    // 2
    #[test]
    fn on_attached_inserts_manifest() {
        let mut registry = RepoRegistry::new();
        let manifest = build_manifest("repo:flux", "flux", "project:flux");
        registry.on_repo_attached(manifest).unwrap();

        assert_eq!(registry.len(), 1);
        let got = registry.get("repo:flux").expect("present");
        assert_eq!(got.slug, "flux");
        assert_eq!(got.repo_role, RepoRole::Source);
        assert_eq!(registry.list_active().len(), 1);
    }

    // 3
    #[test]
    fn on_attached_rejects_duplicate_id() {
        let mut registry = RepoRegistry::new();
        let first = build_manifest("repo:flux", "flux", "project:flux");
        registry.on_repo_attached(first).unwrap();

        let dup = build_manifest("repo:flux", "flux", "project:flux");
        let err = registry.on_repo_attached(dup).unwrap_err();
        assert!(matches!(err, RepoRegistryError::DuplicateAttach(ref id) if id == "repo:flux"));
        assert_eq!(registry.len(), 1);
    }

    // 4
    #[test]
    fn state_transition_active_to_paused() {
        let mut registry = RepoRegistry::new();
        registry
            .on_repo_attached(build_manifest("repo:flux", "flux", "project:flux"))
            .unwrap();
        assert_eq!(registry.list_active().len(), 1);

        registry
            .on_repo_state_changed("repo:flux", RepoState::Paused)
            .unwrap();
        assert_eq!(registry.list_active().len(), 0);
        assert_eq!(registry.get("repo:flux").unwrap().state, RepoState::Paused);
    }

    // 5
    #[test]
    fn state_transition_paused_to_active() {
        let mut registry = RepoRegistry::new();
        registry
            .on_repo_attached(build_manifest("repo:flux", "flux", "project:flux"))
            .unwrap();
        registry
            .on_repo_state_changed("repo:flux", RepoState::Paused)
            .unwrap();
        registry
            .on_repo_state_changed("repo:flux", RepoState::Active)
            .unwrap();
        assert_eq!(registry.get("repo:flux").unwrap().state, RepoState::Active);
        assert_eq!(registry.list_active().len(), 1);
    }

    // 6
    #[test]
    fn state_transition_active_to_detached() {
        let mut registry = RepoRegistry::new();
        registry
            .on_repo_attached(build_manifest("repo:flux", "flux", "project:flux"))
            .unwrap();
        registry
            .on_repo_state_changed("repo:flux", RepoState::Detached)
            .unwrap();
        assert_eq!(
            registry.get("repo:flux").unwrap().state,
            RepoState::Detached
        );
        assert_eq!(registry.list_active().len(), 0);
    }

    // 7
    #[test]
    fn state_transition_paused_to_detached() {
        let mut registry = RepoRegistry::new();
        registry
            .on_repo_attached(build_manifest("repo:flux", "flux", "project:flux"))
            .unwrap();
        registry
            .on_repo_state_changed("repo:flux", RepoState::Paused)
            .unwrap();
        registry
            .on_repo_state_changed("repo:flux", RepoState::Detached)
            .unwrap();
        assert_eq!(
            registry.get("repo:flux").unwrap().state,
            RepoState::Detached
        );
    }

    // 8
    #[test]
    fn state_transition_detached_to_active_rejected() {
        let mut registry = RepoRegistry::new();
        registry
            .on_repo_attached(build_manifest("repo:flux", "flux", "project:flux"))
            .unwrap();
        registry.on_repo_detached("repo:flux").unwrap();

        let err = registry
            .on_repo_state_changed("repo:flux", RepoState::Active)
            .unwrap_err();
        assert!(matches!(
            err,
            RepoRegistryError::InvalidTransition {
                from: RepoState::Detached,
                to: RepoState::Active,
                ..
            }
        ));
        assert_eq!(
            registry.get("repo:flux").unwrap().state,
            RepoState::Detached
        );
    }

    // 9
    #[test]
    fn state_transition_detached_to_paused_rejected() {
        let mut registry = RepoRegistry::new();
        registry
            .on_repo_attached(build_manifest("repo:flux", "flux", "project:flux"))
            .unwrap();
        registry.on_repo_detached("repo:flux").unwrap();

        let err = registry
            .on_repo_state_changed("repo:flux", RepoState::Paused)
            .unwrap_err();
        assert!(matches!(
            err,
            RepoRegistryError::InvalidTransition {
                from: RepoState::Detached,
                to: RepoState::Paused,
                ..
            }
        ));
    }

    // 10
    #[test]
    fn on_detached_sets_state_and_keeps_entry() {
        let mut registry = RepoRegistry::new();
        registry
            .on_repo_attached(build_manifest("repo:flux", "flux", "project:flux"))
            .unwrap();
        registry.on_repo_detached("repo:flux").unwrap();

        let got = registry.get("repo:flux").expect("kept for audit");
        assert_eq!(got.state, RepoState::Detached);
        assert_eq!(registry.list_active().len(), 0);
        assert_eq!(registry.len(), 1);
        assert_eq!(registry.list_all().len(), 1);
    }

    // 11
    #[test]
    fn list_by_project_filters_correctly() {
        let mut registry = RepoRegistry::new();
        registry
            .on_repo_attached(build_manifest("repo:flux", "flux", "project:flux"))
            .unwrap();
        registry
            .on_repo_attached(build_manifest("repo:other", "other", "project:other"))
            .unwrap();

        let flux_only = registry.list_by_project("project:flux");
        assert_eq!(flux_only.len(), 1);
        assert_eq!(flux_only[0].slug, "flux");

        let other_only = registry.list_by_project("project:other");
        assert_eq!(other_only.len(), 1);
        assert_eq!(other_only[0].slug, "other");

        let none = registry.list_by_project("project:missing");
        assert!(none.is_empty());
    }

    // 12
    #[test]
    fn load_from_archive_happy_path() {
        let tmp = TempDir::new().unwrap();
        let project_rel = "operations/projects/foo";

        write_file(
            tmp.path(),
            &format!("{project_rel}/project.md"),
            &project_md("project:foo", "foo", &["bar"]),
        );
        write_file(
            tmp.path(),
            &format!("{project_rel}/repos/bar.md"),
            &source_yaml("repo:bar", "bar", "project:foo"),
        );

        let registry = RepoRegistry::load_from_archive(tmp.path()).unwrap();
        assert_eq!(registry.len(), 1);
        let got = registry.get("repo:bar").expect("present");
        assert_eq!(got.slug, "bar");
        assert_eq!(got.project_id, "project:foo");
    }

    // 13
    #[test]
    fn load_from_archive_rejects_duplicate_id_across_projects() {
        let tmp = TempDir::new().unwrap();

        // Project A declares repo:shared.
        write_file(
            tmp.path(),
            "operations/projects/a/project.md",
            &project_md("project:a", "a", &["shared"]),
        );
        write_file(
            tmp.path(),
            "operations/projects/a/repos/shared.md",
            &source_yaml("repo:shared", "shared", "project:a"),
        );

        // Project B also declares repo:shared.
        write_file(
            tmp.path(),
            "operations/projects/b/project.md",
            &project_md("project:b", "b", &["shared"]),
        );
        write_file(
            tmp.path(),
            "operations/projects/b/repos/shared.md",
            &source_yaml("repo:shared", "shared", "project:b"),
        );

        let err = RepoRegistry::load_from_archive(tmp.path()).unwrap_err();
        let downcast = err
            .downcast_ref::<RepoRegistryError>()
            .expect("expected RepoRegistryError in chain");
        assert!(matches!(
            downcast,
            RepoRegistryError::DuplicateAttach(id) if id == "repo:shared"
        ));
    }
}
