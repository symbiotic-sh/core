//! Wrapper around `AccessBroker` that composes `RepoManifest.repo_role` +
//! `agent_scopes` into a single-action `CapabilityToken`. Primary enforcement
//! point per `docs/design/repo-manifest.md` §Resolution Rules §6.
//!
//! The wrapper layer keeps `symbiotic-trust` stable: the role-gate pre-filter
//! and scope composition live here at the daemon layer, and the composed token
//! is handed to the existing `AccessBroker::issue_token` / `evaluate` pipeline
//! without extending the trust-layer API.
//!
//! `#![allow(dead_code)]`: the types and helpers in this module land ahead of
//! their consumers per the T126 chunk plan — `issue_repo_token`, `RepoAction`,
//! `RepoCapabilityError`, and the `action_tag` helper are exercised by tests
//! but not yet called from the daemon's hot path. Integration with the push /
//! recall / write paths lands in follow-up chunks.
#![allow(dead_code)]

use std::collections::HashSet;

use symbiotic_control_plane::{RepoManifest, RepoRole};
use symbiotic_trust::{AccessBroker, AgentTrustLevel, CapabilityToken};
use thiserror::Error;
use uuid::Uuid;

/// What the agent wants to do against a repo. The role gate maps each action
/// to an allow/deny + a scope set derived from `RepoManifest.agent_scopes`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepoAction {
    /// Read-only worktree access (clone/worktree/fetch into agent sandbox).
    Read,
    /// Write access (commit into ephemeral per-goal bare repo).
    Write,
    /// Push from `mirror.internal_bare_path` to `source.url` (external push).
    PushExternal,
    /// Read-only query into the AKB derived index (T129). Only granted when
    /// `repo_role != Source`.
    AkbQuery,
}

/// Errors surfaced by `issue_repo_token`.
///
/// `IndexingMissing` is declared here for symmetry with the surrounding error
/// surface, but this module does not actually produce it — the load-time check
/// for role ↔ indexing coupling lives in `RepoManifest::validate()` (chunk §02).
#[derive(Debug, Error, PartialEq, Eq)]
pub enum RepoCapabilityError {
    #[error("role gate denied: repo_role={role:?} does not permit action={action:?}")]
    RoleGateDenied { role: RepoRole, action: RepoAction },
    #[error("push_external disabled on manifest {id}")]
    PushExternalDisabled { id: String },
    #[error("indexing block missing on non-source repo {id}")]
    IndexingMissing { id: String },
}

/// Issue a single-action capability token against the given repo manifest.
///
/// Composes the manifest's `repo_role` + `agent_scopes` with the agent's
/// trust ceiling and hands the resulting `CapabilityToken` to the caller.
/// The token is also registered on the broker — callers pass `token.token_id`
/// to `AccessBroker::evaluate` at request time.
///
/// This is the primary enforcement point for `docs/design/repo-manifest.md`
/// §Resolution Rules §6 ("role gate"). If the operator's trust ceiling is
/// below the action's minimum, `AccessBroker::evaluate` will reject the
/// token at request time — this function does NOT down-issue to the ceiling;
/// the token's `trust_level` is exactly the provided ceiling.
pub fn issue_repo_token(
    broker: &mut AccessBroker,
    manifest: &RepoManifest,
    subject: &str,
    action: RepoAction,
    trust_ceiling: AgentTrustLevel,
    now: u64,
    ttl_secs: u64,
    goal_scope: Option<&str>,
) -> Result<CapabilityToken, RepoCapabilityError> {
    // ── Role gate ──────────────────────────────────────────────────────
    // Single source of truth for which (role, action) pairs are allowed.
    // See docs/design/repo-manifest.md §Resolution Rules §6.
    let role = manifest.repo_role;
    match (role, action) {
        // Source: read/write always allowed; push gated by push_external;
        // AkbQuery rejected (no AKB exists for source repos).
        (RepoRole::Source, RepoAction::Read) => {}
        (RepoRole::Source, RepoAction::Write) => {}
        (RepoRole::Source, RepoAction::PushExternal) => {
            if !manifest.agent_scopes.push_external {
                return Err(RepoCapabilityError::PushExternalDisabled {
                    id: manifest.id.clone(),
                });
            }
        }
        (RepoRole::Source, RepoAction::AkbQuery) => {
            return Err(RepoCapabilityError::RoleGateDenied { role, action });
        }
        // Non-Source roles: only AkbQuery is permitted (read-only AKB index).
        (RepoRole::DocsAkb, RepoAction::AkbQuery)
        | (RepoRole::ReferenceLibrary, RepoAction::AkbQuery) => {}
        (RepoRole::DocsAkb, _) | (RepoRole::ReferenceLibrary, _) => {
            return Err(RepoCapabilityError::RoleGateDenied { role, action });
        }
    }

    // ── Scope composition ──────────────────────────────────────────────
    let scopes: HashSet<String> = match action {
        RepoAction::Read => manifest.agent_scopes.read.iter().cloned().collect(),
        RepoAction::Write => manifest.agent_scopes.write.iter().cloned().collect(),
        RepoAction::PushExternal => manifest
            .agent_scopes
            .write
            .iter()
            .cloned()
            .chain(std::iter::once("git.push_external".to_string()))
            .collect(),
        RepoAction::AkbQuery => {
            let mut s = HashSet::new();
            s.insert("akb.query".to_string());
            s
        }
    };

    // ── Token construction ─────────────────────────────────────────────
    let token_id = format!(
        "repo:{}:{}:{}",
        manifest.slug,
        action_tag(action),
        Uuid::new_v4()
    );

    let token = CapabilityToken {
        token_id,
        subject: subject.to_string(),
        trust_level: trust_ceiling,
        scopes,
        expires_at: now + ttl_secs,
        one_time: false,
        consumed: false,
        goal_scope: goal_scope.map(String::from),
    };

    broker.issue_token(token.clone());
    Ok(token)
}

/// Map `RepoAction` to its canonical token-id tag.
fn action_tag(action: RepoAction) -> &'static str {
    match action {
        RepoAction::Read => "read",
        RepoAction::Write => "write",
        RepoAction::PushExternal => "push",
        RepoAction::AkbQuery => "akb",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use symbiotic_control_plane::{
        AkbTier, CredentialScope, MirrorDirection, RepoAgentScopes, RepoCheckoutPolicy,
        RepoCredentialBinding, RepoDistilleryConfig, RepoHooks, RepoIndexingPolicy, RepoMetadata,
        RepoMirrorPolicy, RepoProvider, RepoRefreshPolicy, RepoSource, RepoState, RepoTierPolicy,
    };
    use symbiotic_trust::{AccessRequest, BrokerError};

    fn default_indexing_policy() -> RepoIndexingPolicy {
        RepoIndexingPolicy {
            root_paths: vec!["README.md".to_string()],
            exclude_patterns: vec![],
            distillery_config: RepoDistilleryConfig {
                enable_reweave: false,
                enable_semantic_verify: false,
                model: "test-model".to_string(),
            },
            tier_policy: RepoTierPolicy {
                default_tier: AkbTier::Distilled,
                auto_promote: false,
            },
            refresh: RepoRefreshPolicy {
                on_commit: false,
                interval_secs: 3600,
                incremental: true,
            },
        }
    }

    fn build_manifest(role: RepoRole, push_external: bool) -> RepoManifest {
        let slug = "flux";
        let indexing = match role {
            RepoRole::Source => None,
            RepoRole::DocsAkb | RepoRole::ReferenceLibrary => Some(default_indexing_policy()),
        };
        RepoManifest {
            id: format!("repo:{slug}"),
            project_id: "project:flux".to_string(),
            slug: slug.to_string(),
            title: "Flux".to_string(),
            state: RepoState::Active,
            repo_role: role,
            source: RepoSource {
                url: format!("git@github.com:example/{slug}.git"),
                provider: RepoProvider::Github,
                default_branch: "main".to_string(),
                protected_branches: vec!["main".to_string()],
                pinned_head: None,
            },
            credential: RepoCredentialBinding {
                id: format!("credential:example-{slug}-push"),
                scope: CredentialScope::Push,
                trust_floor: AgentTrustLevel::CredentialAccess,
            },
            mirror: RepoMirrorPolicy {
                internal_bare_path: PathBuf::from(format!("data/git-server/repos/{slug}.git")),
                direction: MirrorDirection::Bidirectional,
                sync_interval_secs: 300,
                last_pulled_at: None,
                last_pushed_at: None,
            },
            checkout: RepoCheckoutPolicy {
                worktree_root: PathBuf::from(format!("data/worktrees/{slug}/")),
                agent_branch_prefix: "agent/".to_string(),
                max_concurrent_worktrees: 4,
                cleanup_on_goal_close: true,
            },
            agent_scopes: RepoAgentScopes {
                read: vec!["archive.read".to_string(), "file.read".to_string()],
                write: vec![
                    "archive.read".to_string(),
                    "archive.write".to_string(),
                    "file.read".to_string(),
                    "file.write".to_string(),
                ],
                push_external,
                requires_operator_approval_for: if push_external {
                    vec!["push_external".to_string()]
                } else {
                    vec![]
                },
            },
            hooks: RepoHooks {
                on_attach: None,
                on_drift_detected: None,
                on_detach: None,
            },
            indexing,
            metadata: RepoMetadata {
                attached_at: "2026-04-17T00:00:00Z".to_string(),
                attached_by: "operator".to_string(),
                notes: String::new(),
            },
            archeology_policy: None,
            body_markdown: String::new(),
        }
    }

    // (1)
    #[test]
    fn source_read_issues_token_with_read_scopes() {
        let manifest = build_manifest(RepoRole::Source, false);
        let mut broker = AccessBroker::new();
        let token = issue_repo_token(
            &mut broker,
            &manifest,
            "agent-a",
            RepoAction::Read,
            AgentTrustLevel::ArchiveWrite,
            1_000,
            3600,
            None,
        )
        .expect("read should issue");
        assert!(token.scopes.contains("archive.read"));
        assert!(token.scopes.contains("file.read"));
        assert!(token.token_id.starts_with("repo:flux:read:"));
    }

    // (2)
    #[test]
    fn source_write_issues_token_with_write_scopes() {
        let manifest = build_manifest(RepoRole::Source, false);
        let mut broker = AccessBroker::new();
        let token = issue_repo_token(
            &mut broker,
            &manifest,
            "agent-a",
            RepoAction::Write,
            AgentTrustLevel::ArchiveWrite,
            1_000,
            3600,
            None,
        )
        .expect("write should issue");
        assert!(token.scopes.contains("archive.read"));
        assert!(token.scopes.contains("archive.write"));
        assert!(token.scopes.contains("file.read"));
        assert!(token.scopes.contains("file.write"));
        assert!(token.token_id.starts_with("repo:flux:write:"));
    }

    // (3)
    #[test]
    fn source_push_external_enabled_issues_token_with_push_scope() {
        let manifest = build_manifest(RepoRole::Source, true);
        let mut broker = AccessBroker::new();
        let token = issue_repo_token(
            &mut broker,
            &manifest,
            "agent-a",
            RepoAction::PushExternal,
            AgentTrustLevel::ExternalAct,
            1_000,
            3600,
            None,
        )
        .expect("push should issue");
        assert!(token.scopes.contains("git.push_external"));
        assert!(token.scopes.contains("archive.write"));
        assert!(token.token_id.starts_with("repo:flux:push:"));
    }

    // (4)
    #[test]
    fn source_push_external_disabled_rejects() {
        let manifest = build_manifest(RepoRole::Source, false);
        let mut broker = AccessBroker::new();
        let err = issue_repo_token(
            &mut broker,
            &manifest,
            "agent-a",
            RepoAction::PushExternal,
            AgentTrustLevel::ExternalAct,
            1_000,
            3600,
            None,
        )
        .expect_err("push with push_external=false must reject");
        assert_eq!(
            err,
            RepoCapabilityError::PushExternalDisabled {
                id: "repo:flux".to_string()
            }
        );
    }

    // (5)
    #[test]
    fn source_akb_query_rejects() {
        let manifest = build_manifest(RepoRole::Source, false);
        let mut broker = AccessBroker::new();
        let err = issue_repo_token(
            &mut broker,
            &manifest,
            "agent-a",
            RepoAction::AkbQuery,
            AgentTrustLevel::ReadOnly,
            1_000,
            3600,
            None,
        )
        .expect_err("akb query on source must reject");
        assert_eq!(
            err,
            RepoCapabilityError::RoleGateDenied {
                role: RepoRole::Source,
                action: RepoAction::AkbQuery,
            }
        );
    }

    // (6)
    #[test]
    fn docs_akb_read_rejects() {
        let manifest = build_manifest(RepoRole::DocsAkb, false);
        let mut broker = AccessBroker::new();
        let err = issue_repo_token(
            &mut broker,
            &manifest,
            "agent-a",
            RepoAction::Read,
            AgentTrustLevel::ReadOnly,
            1_000,
            3600,
            None,
        )
        .expect_err("read on docs_akb must reject");
        assert_eq!(
            err,
            RepoCapabilityError::RoleGateDenied {
                role: RepoRole::DocsAkb,
                action: RepoAction::Read,
            }
        );
    }

    // (7)
    #[test]
    fn docs_akb_akb_query_issues_token() {
        let manifest = build_manifest(RepoRole::DocsAkb, false);
        let mut broker = AccessBroker::new();
        let token = issue_repo_token(
            &mut broker,
            &manifest,
            "agent-a",
            RepoAction::AkbQuery,
            AgentTrustLevel::ReadOnly,
            1_000,
            3600,
            None,
        )
        .expect("akb query on docs_akb should issue");
        let expected: HashSet<String> = ["akb.query".to_string()].into_iter().collect();
        assert_eq!(token.scopes, expected);
        assert!(token.token_id.starts_with("repo:flux:akb:"));
    }

    // (8)
    #[test]
    fn reference_library_push_external_rejects() {
        let manifest = build_manifest(RepoRole::ReferenceLibrary, true);
        let mut broker = AccessBroker::new();
        let err = issue_repo_token(
            &mut broker,
            &manifest,
            "agent-a",
            RepoAction::PushExternal,
            AgentTrustLevel::ExternalAct,
            1_000,
            3600,
            None,
        )
        .expect_err("push on reference_library must reject");
        assert_eq!(
            err,
            RepoCapabilityError::RoleGateDenied {
                role: RepoRole::ReferenceLibrary,
                action: RepoAction::PushExternal,
            }
        );
    }

    // (9)
    #[test]
    fn reference_library_akb_query_issues_token() {
        let manifest = build_manifest(RepoRole::ReferenceLibrary, false);
        let mut broker = AccessBroker::new();
        let token = issue_repo_token(
            &mut broker,
            &manifest,
            "agent-a",
            RepoAction::AkbQuery,
            AgentTrustLevel::ReadOnly,
            1_000,
            3600,
            None,
        )
        .expect("akb query on reference_library should issue");
        let expected: HashSet<String> = ["akb.query".to_string()].into_iter().collect();
        assert_eq!(token.scopes, expected);
        assert!(token.token_id.starts_with("repo:flux:akb:"));
    }

    // (10)
    #[test]
    fn issued_token_passes_broker_evaluate() {
        let manifest = build_manifest(RepoRole::Source, false);
        let mut broker = AccessBroker::new();
        let now = 1_000u64;
        let token = issue_repo_token(
            &mut broker,
            &manifest,
            "agent-a",
            RepoAction::Read,
            AgentTrustLevel::ArchiveWrite,
            now,
            3600,
            None,
        )
        .expect("read should issue");

        let decision = broker
            .evaluate(
                &token.token_id,
                &AccessRequest {
                    subject: "agent-a".to_string(),
                    required_level: AgentTrustLevel::ArchiveWrite,
                    scope: "archive.read".to_string(),
                    goal_scope: None,
                },
                now,
            )
            .expect("broker should grant");
        assert!(decision.allowed);
    }

    // (11)
    #[test]
    fn issued_token_fails_broker_evaluate_on_insufficient_trust() {
        let manifest = build_manifest(RepoRole::Source, false);
        let mut broker = AccessBroker::new();
        let now = 1_000u64;
        let token = issue_repo_token(
            &mut broker,
            &manifest,
            "agent-a",
            RepoAction::Read,
            AgentTrustLevel::ReadOnly,
            now,
            3600,
            None,
        )
        .expect("read should issue");

        let err = broker
            .evaluate(
                &token.token_id,
                &AccessRequest {
                    subject: "agent-a".to_string(),
                    required_level: AgentTrustLevel::ArchiveWrite,
                    scope: "archive.read".to_string(),
                    goal_scope: None,
                },
                now,
            )
            .expect_err("broker should deny on insufficient trust");
        let broker_err = err
            .downcast_ref::<BrokerError>()
            .expect("should be BrokerError");
        assert!(
            matches!(broker_err, BrokerError::InsufficientTrust),
            "expected InsufficientTrust, got {broker_err:?}"
        );
    }

    // (12)
    #[test]
    fn token_id_includes_manifest_slug_and_action_tag() {
        let source = build_manifest(RepoRole::Source, true);
        let docs = build_manifest(RepoRole::DocsAkb, false);
        let mut broker = AccessBroker::new();

        let read = issue_repo_token(
            &mut broker,
            &source,
            "a",
            RepoAction::Read,
            AgentTrustLevel::ArchiveWrite,
            1_000,
            3600,
            None,
        )
        .expect("read");
        let write = issue_repo_token(
            &mut broker,
            &source,
            "a",
            RepoAction::Write,
            AgentTrustLevel::ArchiveWrite,
            1_000,
            3600,
            None,
        )
        .expect("write");
        let push = issue_repo_token(
            &mut broker,
            &source,
            "a",
            RepoAction::PushExternal,
            AgentTrustLevel::ExternalAct,
            1_000,
            3600,
            None,
        )
        .expect("push");
        let akb = issue_repo_token(
            &mut broker,
            &docs,
            "a",
            RepoAction::AkbQuery,
            AgentTrustLevel::ReadOnly,
            1_000,
            3600,
            None,
        )
        .expect("akb");

        assert!(read.token_id.contains(":read:"));
        assert!(write.token_id.contains(":write:"));
        assert!(push.token_id.contains(":push:"));
        assert!(akb.token_id.contains(":akb:"));
        assert!(read.token_id.contains("repo:flux:"));
    }
}
