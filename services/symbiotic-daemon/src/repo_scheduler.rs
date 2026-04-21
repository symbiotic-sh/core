//! Per-repo mirror scheduler (T126 §08.c).
//!
//! Spawns one `tokio::task` per active repo in the registry. Each task drives
//! the mirror loop on the manifest's `mirror.sync_interval_secs` cadence:
//! inspect git state to decide among `Pull` / `Push` / `Noop` / `Conflict`,
//! then execute.
//!
//! Why `decide` rather than "pull unconditionally then push if ahead":
//! `mirror_pull_once` uses `+refs/heads/*:refs/heads/*` (force-fetch). An
//! unconditional pull would destroy any unpushed local commits on
//! `refs/heads/{default_branch}`. Deciding up front via `git ls-remote` +
//! `merge-base --is-ancestor` is non-destructive and lets us detect true
//! divergence (conflicts) rather than silently clobbering local work.
//!
//! Scheduler is read-only against `RepoRegistry`: it snapshots the active
//! manifest list once at startup. Dynamic attach/detach handling is out of
//! scope for this chunk — the `repo_attached` / `repo_detached` events feed
//! the registry, but re-spawning the scheduler is a follow-up.
//!
//! The daemon is `!Send`, so scheduler tasks cannot hold a `&SymbioticDaemon`.
//! Conflict-resolution goals are enqueued via `ConflictGoalSender` and
//! dispatched by the pump loop (which owns `!Send` state) in `main.rs`.
//! Matrix events flow through `DaemonMatrixPoster` + the §08.b outbound
//! channel for the same reason.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use symbiotic_control_plane::RepoManifest;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::approval_gate::ApprovalGate;
use crate::matrix_poster::DaemonMatrixPoster;
use crate::push::PushProvider;
use crate::repo_events;
use crate::repo_mirror::{self, RepoMirrorArchiveContext};
use crate::{ConflictGoalRequest, ConflictGoalSender, MatrixOutboundSender};

/// Shared, cheap-to-clone dependencies threaded into each per-repo task.
pub(crate) struct SchedulerDeps {
    pub registry: crate::repo_registry::SharedRepoRegistry,
    pub approval_gate: Arc<std::sync::Mutex<ApprovalGate>>,
    pub credential_vault: Arc<credential_gateway::GoalScopedVault>,
    pub push_provider: Arc<dyn PushProvider>,
    pub matrix_outbound_tx: MatrixOutboundSender,
    pub conflict_goal_tx: ConflictGoalSender,
    pub operator_room_id: String,
    pub project_goals_room: String,
    pub archive_root: PathBuf,
    pub agent_id: String,
    pub approval_ttl_secs: u64,
    pub approval_poll_interval_ms: u64,
}

/// Per-task frozen view of `SchedulerDeps`. All fields are owned clones so a
/// task can run on any executor thread without borrowing the parent.
struct TaskDeps {
    approval_gate: Arc<std::sync::Mutex<ApprovalGate>>,
    credential_vault: Arc<credential_gateway::GoalScopedVault>,
    push_provider: Arc<dyn PushProvider>,
    matrix_outbound_tx: MatrixOutboundSender,
    conflict_goal_tx: ConflictGoalSender,
    operator_room_id: String,
    project_goals_room: String,
    archive_root: PathBuf,
    agent_id: String,
    approval_ttl_secs: u64,
    approval_poll_interval_ms: u64,
}

impl TaskDeps {
    fn from_shared(src: &SchedulerDeps) -> Self {
        Self {
            approval_gate: src.approval_gate.clone(),
            credential_vault: src.credential_vault.clone(),
            push_provider: src.push_provider.clone(),
            matrix_outbound_tx: src.matrix_outbound_tx.clone(),
            conflict_goal_tx: src.conflict_goal_tx.clone(),
            operator_room_id: src.operator_room_id.clone(),
            project_goals_room: src.project_goals_room.clone(),
            archive_root: src.archive_root.clone(),
            agent_id: src.agent_id.clone(),
            approval_ttl_secs: src.approval_ttl_secs,
            approval_poll_interval_ms: src.approval_poll_interval_ms,
        }
    }
}

/// Decision produced by `decide` for each scheduler tick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LoopAction {
    /// Local and remote are already in sync — skip this tick.
    Noop,
    /// Pull from remote. Either the local bare is uninitialized, or the
    /// local head is an ancestor of the remote head (fast-forward-safe).
    Pull,
    /// Local has commits ahead of remote. Run `mirror_push_with_approval`.
    Push,
    /// Local and remote have diverged. Emit a `repo_mirror_conflict_opened`
    /// event and enqueue an agentic conflict-resolution goal.
    Conflict,
}

/// Spawn one `tokio::task` per manifest. Each task runs until its
/// `cancel_rx` receiver observes `true`.
pub(crate) fn spawn_per_repo_scheduler(
    manifests: Vec<RepoManifest>,
    deps: SchedulerDeps,
    cancel_rx: watch::Receiver<bool>,
) -> Vec<JoinHandle<()>> {
    manifests
        .into_iter()
        .map(|manifest| {
            let task_deps = TaskDeps::from_shared(&deps);
            let cancel = cancel_rx.clone();
            tokio::spawn(run_repo_task(manifest, task_deps, cancel))
        })
        .collect()
}

/// Convenience wrapper: snapshot active manifests from the registry and spawn.
pub(crate) async fn spawn_from_registry(
    deps: SchedulerDeps,
    cancel_rx: watch::Receiver<bool>,
) -> Vec<JoinHandle<()>> {
    let manifests = {
        let registry = deps.registry.lock().await;
        registry.list_active().into_iter().cloned().collect()
    };
    spawn_per_repo_scheduler(manifests, deps, cancel_rx)
}

/// Top-level per-repo task body. Loop until cancellation.
async fn run_repo_task(
    manifest: RepoManifest,
    deps: TaskDeps,
    mut cancel_rx: watch::Receiver<bool>,
) {
    // Floor the sleep interval at 1s so a misconfigured 0 doesn't busy-loop.
    let interval = Duration::from_secs(manifest.mirror.sync_interval_secs.max(1));

    loop {
        tokio::select! {
            _ = cancel_rx.changed() => {
                if *cancel_rx.borrow() {
                    tracing::debug!(repo_id = %manifest.id, "repo_scheduler: cancellation received, exiting");
                    return;
                }
            }
            _ = tokio::time::sleep(interval) => {}
        }

        run_one_tick(&manifest, &deps).await;
    }
}

/// Run exactly one decide → act cycle. Exposed for tests.
async fn run_one_tick(manifest: &RepoManifest, deps: &TaskDeps) {
    let action = {
        let manifest_cl = manifest.clone();
        match tokio::task::spawn_blocking(move || decide(&manifest_cl)).await {
            Ok(a) => a,
            Err(e) => {
                tracing::warn!(
                    repo_id = %manifest.id,
                    "repo_scheduler: decide join failed: {e}"
                );
                return;
            }
        }
    };

    match action {
        LoopAction::Noop => {}
        LoopAction::Pull => do_pull(manifest, deps).await,
        LoopAction::Push => do_push(manifest, deps).await,
        LoopAction::Conflict => do_conflict(manifest, deps),
    }
}

async fn do_pull(manifest: &RepoManifest, deps: &TaskDeps) {
    let manifest_cl = manifest.clone();
    let archive_root_cl = deps.archive_root.clone();
    let project_id_cl = manifest.project_id.clone();
    let now = now_unix_i64();
    let res = tokio::task::spawn_blocking(move || {
        let ctx = RepoMirrorArchiveContext {
            archive_root: &archive_root_cl,
            project_id: &project_id_cl,
            observed_at: now,
        };
        repo_mirror::mirror_pull_once(&manifest_cl, Some(&ctx))
    })
    .await;
    match res {
        Ok(Ok(report)) => {
            tracing::debug!(
                repo_id = %manifest.id,
                initialized = report.initialized,
                refs_advanced = report.refs_advanced,
                "repo_scheduler: pull ok"
            );
        }
        Ok(Err(e)) => {
            tracing::warn!(repo_id = %manifest.id, "repo_scheduler: pull failed: {e}");
        }
        Err(join_err) => {
            tracing::warn!(repo_id = %manifest.id, "repo_scheduler: pull join failed: {join_err}");
        }
    }
}

async fn do_push(manifest: &RepoManifest, deps: &TaskDeps) {
    let poster = DaemonMatrixPoster::new(deps.matrix_outbound_tx.clone());
    let ctx = RepoMirrorArchiveContext {
        archive_root: deps.archive_root.as_path(),
        project_id: manifest.project_id.as_str(),
        observed_at: now_unix_i64(),
    };
    let res = repo_mirror::mirror_push_with_approval(
        manifest,
        deps.agent_id.as_str(),
        &deps.credential_vault,
        &deps.approval_gate,
        &poster,
        deps.push_provider.as_ref(),
        deps.operator_room_id.as_str(),
        Some(&ctx),
        manifest.source.default_branch.as_str(),
        None,
        None,
        deps.approval_ttl_secs,
        deps.approval_poll_interval_ms,
    )
    .await;
    match res {
        Ok(report) => {
            tracing::debug!(repo_id = %manifest.id, pushed = report.pushed, "repo_scheduler: push ok");
        }
        Err(e) => {
            tracing::warn!(repo_id = %manifest.id, "repo_scheduler: push failed: {e}");
        }
    }
}

fn do_conflict(manifest: &RepoManifest, deps: &TaskDeps) {
    let branch = manifest.source.default_branch.as_str();
    let observed_at = now_unix_i64();
    let payload = format!(
        "branch: \"{branch}\"\ndetected_by: \"scheduler\"\n",
        branch = branch,
    );
    let body = format!(
        "Mirror conflict on `{id}`:`{branch}`: local and remote have diverged. Agentic conflict goal enqueued.\n",
        id = manifest.id,
        branch = branch,
    );
    if let Err(e) = repo_events::append_repo_event_archive(
        deps.archive_root.as_path(),
        manifest.project_id.as_str(),
        manifest.id.as_str(),
        "repo_mirror_conflict_opened",
        observed_at,
        manifest.slug.as_str(),
        &payload,
        &body,
    ) {
        tracing::warn!(
            repo_id = %manifest.id,
            "repo_scheduler: failed to emit repo_mirror_conflict_opened: {e}"
        );
    }

    let description = format!(
        "Resolve mirror conflict on {id}:{branch}",
        id = manifest.id,
        branch = branch,
    );
    let req = ConflictGoalRequest {
        description,
        room_id: deps.project_goals_room.clone(),
        sender: deps.agent_id.clone(),
    };
    if let Err(e) = deps.conflict_goal_tx.send(req) {
        tracing::warn!(
            repo_id = %manifest.id,
            "repo_scheduler: conflict_goal_tx send failed (receiver dropped?): {e}"
        );
    }
}

// ── Decide helper ─────────────────────────────────────────────────────────

/// Inspect git state non-destructively and choose an action for this tick.
///
/// Steps:
/// 1. If the local bare isn't initialized yet → `Pull` (bootstrap).
/// 2. Otherwise, do a non-destructive fetch into `refs/remotes/origin/*`
///    so both local and remote commit objects are reachable for merge-base
///    queries. `refs/heads/*` is NOT touched here — that's what makes the
///    decision safe when local has unpushed commits on `refs/heads/{branch}`.
/// 3. Compare `refs/heads/{branch}` (local) vs. `refs/remotes/origin/{branch}`
///    (remote tip) and choose `Noop` / `Pull` / `Push` / `Conflict`.
///
/// Runs on the blocking threadpool since it shells out to `git`.
fn decide(manifest: &RepoManifest) -> LoopAction {
    let bare_path = manifest.mirror.internal_bare_path.as_path();
    let branch = manifest.source.default_branch.as_str();
    let local_ref = format!("refs/heads/{branch}");
    let tracking_ref = format!("refs/remotes/origin/{branch}");

    // 1. Bootstrap.
    if !is_bare_repo(bare_path) {
        return LoopAction::Pull;
    }

    // 2. Non-destructive fetch. If it fails (network, bad creds, etc.), we
    //    conservatively no-op this tick — do not risk classifying as Conflict
    //    when we simply couldn't see the remote.
    if !fetch_into_remote_tracking(bare_path, manifest.source.url.as_str()) {
        tracing::debug!(
            repo_id = %manifest.id,
            "repo_scheduler: non-destructive fetch failed; skipping tick"
        );
        return LoopAction::Noop;
    }

    let local = rev_parse(bare_path, &local_ref);
    let remote = rev_parse(bare_path, &tracking_ref);

    match (local.as_deref(), remote.as_deref()) {
        // Local branch missing but remote has it → Pull (populates refs/heads/{branch}).
        (None, Some(_)) => LoopAction::Pull,
        // Local has the branch, remote doesn't → Push if allowed.
        (Some(_), None) => {
            if manifest.agent_scopes.push_external {
                LoopAction::Push
            } else {
                LoopAction::Noop
            }
        }
        // Neither side has it → nothing to do.
        (None, None) => LoopAction::Noop,
        // Identical heads.
        (Some(l), Some(r)) if l == r => LoopAction::Noop,
        // Heads differ — classify via merge-base.
        (Some(l), Some(r)) => {
            let local_ancestor_of_remote = is_ancestor(bare_path, l, r);
            let remote_ancestor_of_local = is_ancestor(bare_path, r, l);
            match (local_ancestor_of_remote, remote_ancestor_of_local) {
                (true, _) => LoopAction::Pull,
                (_, true) => {
                    if manifest.agent_scopes.push_external {
                        LoopAction::Push
                    } else {
                        LoopAction::Noop
                    }
                }
                _ => LoopAction::Conflict,
            }
        }
    }
}

/// Fetch remote heads into `refs/remotes/origin/*` without touching
/// `refs/heads/*`. `--prune` keeps deleted remote branches from lingering.
/// Returns `true` on success, `false` on any error (decide will then no-op).
fn fetch_into_remote_tracking(bare_path: &Path, source_url: &str) -> bool {
    let out = Command::new("git")
        .arg("-C")
        .arg(bare_path)
        .arg("fetch")
        .arg(source_url)
        .arg("+refs/heads/*:refs/remotes/origin/*")
        .arg("--prune")
        .arg("--quiet")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output();
    matches!(out, Ok(o) if o.status.success())
}

fn is_bare_repo(path: &Path) -> bool {
    let probe = Command::new("git")
        .arg("-C")
        .arg(path)
        .arg("rev-parse")
        .arg("--is-bare-repository")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output();
    match probe {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout).trim() == "true",
        _ => false,
    }
}

fn rev_parse(bare_path: &Path, ref_name: &str) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(bare_path)
        .arg("rev-parse")
        .arg("--verify")
        .arg(ref_name)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// True iff `ancestor` is an ancestor of `descendant` in the bare repo at
/// `bare_path`. False on any git error (conservative: a failed probe should
/// not spuriously classify as ancestor).
fn is_ancestor(bare_path: &Path, ancestor: &str, descendant: &str) -> bool {
    let out = Command::new("git")
        .arg("-C")
        .arg(bare_path)
        .arg("merge-base")
        .arg("--is-ancestor")
        .arg(ancestor)
        .arg(descendant)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output();
    matches!(out, Ok(o) if o.status.success())
}

fn now_unix_i64() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// ── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::matrix_poster::MatrixPoster;
    use crate::repo_registry::RepoRegistry;
    use credential_gateway::{CredentialRecord, GoalScopedVault};
    use std::path::PathBuf;
    use symbiotic_control_plane::{
        CredentialScope, MirrorDirection, RepoAgentScopes, RepoCheckoutPolicy,
        RepoCredentialBinding, RepoHooks, RepoMetadata, RepoMirrorPolicy, RepoProvider, RepoRole,
        RepoSource, RepoState,
    };
    use symbiotic_trust::AgentTrustLevel;
    use tempfile::TempDir;
    use tokio::sync::mpsc::unbounded_channel;

    // ── Fixture builders ──────────────────────────────────────────────────

    fn git_env(cmd: &mut Command) {
        cmd.env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null");
    }

    fn must_succeed(mut cmd: Command, what: &str) {
        let out = cmd.output().unwrap_or_else(|e| panic!("spawn {what}: {e}"));
        assert!(
            out.status.success(),
            "{what} failed: status={}, stderr={}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// Initialize a bare repo with a seed commit on `main`. Returns the bare
    /// path; HEAD sha is left on `refs/heads/main`.
    ///
    /// `--initial-branch=main` is explicit because some CI runners (Ubuntu
    /// Actions images) default `init.defaultBranch` to `master`. Without the
    /// pin, the bare's HEAD symref points at `refs/heads/master` and
    /// subsequent clones land with a detached HEAD, breaking downstream
    /// `git push origin main` steps with "src refspec main does not match any".
    fn init_bare_with_commit(dir: &Path, name: &str, content: &str) -> PathBuf {
        let bare = dir.join(format!("{name}.git"));
        let mut init = Command::new("git");
        init.arg("init")
            .arg("--bare")
            .arg("--initial-branch=main")
            .arg(&bare);
        git_env(&mut init);
        must_succeed(init, "init --bare");

        let work = dir.join(format!("{name}-work"));
        let mut clone = Command::new("git");
        clone.arg("clone").arg(&bare).arg(&work);
        git_env(&mut clone);
        must_succeed(clone, "clone seed");

        std::fs::write(work.join("README.md"), content).unwrap();

        let mut checkout = Command::new("git");
        checkout
            .arg("-C")
            .arg(&work)
            .arg("checkout")
            .arg("-B")
            .arg("main");
        git_env(&mut checkout);
        must_succeed(checkout, "checkout -B main");

        let mut add = Command::new("git");
        add.arg("-C").arg(&work).arg("add").arg("README.md");
        git_env(&mut add);
        must_succeed(add, "git add");

        let mut cfg_name = Command::new("git");
        cfg_name
            .arg("-C")
            .arg(&work)
            .arg("config")
            .arg("user.name")
            .arg("Test");
        git_env(&mut cfg_name);
        must_succeed(cfg_name, "config user.name");

        let mut cfg_email = Command::new("git");
        cfg_email
            .arg("-C")
            .arg(&work)
            .arg("config")
            .arg("user.email")
            .arg("test@example.com");
        git_env(&mut cfg_email);
        must_succeed(cfg_email, "config user.email");

        let mut commit = Command::new("git");
        commit
            .arg("-C")
            .arg(&work)
            .arg("commit")
            .arg("-m")
            .arg("seed");
        git_env(&mut commit);
        must_succeed(commit, "git commit");

        let mut push = Command::new("git");
        push.arg("-C")
            .arg(&work)
            .arg("push")
            .arg("origin")
            .arg("main");
        git_env(&mut push);
        must_succeed(push, "git push seed");

        bare
    }

    /// Add another commit on `main` in the bare via a throwaway worktree and
    /// return the new HEAD sha.
    fn add_commit_to_bare(bare: &Path, tmp: &TempDir, content: &str) -> String {
        let work = tmp
            .path()
            .join(format!("edit-{}", uuid::Uuid::new_v4().as_simple()));
        let mut clone = Command::new("git");
        clone.arg("clone").arg(bare).arg(&work);
        git_env(&mut clone);
        must_succeed(clone, "clone edit");

        std::fs::write(work.join("README.md"), content).unwrap();

        let mut cfg_name = Command::new("git");
        cfg_name
            .arg("-C")
            .arg(&work)
            .arg("config")
            .arg("user.name")
            .arg("Test");
        git_env(&mut cfg_name);
        must_succeed(cfg_name, "config user.name");

        let mut cfg_email = Command::new("git");
        cfg_email
            .arg("-C")
            .arg(&work)
            .arg("config")
            .arg("user.email")
            .arg("test@example.com");
        git_env(&mut cfg_email);
        must_succeed(cfg_email, "config user.email");

        let mut add = Command::new("git");
        add.arg("-C").arg(&work).arg("add").arg("README.md");
        git_env(&mut add);
        must_succeed(add, "git add edit");

        let mut commit = Command::new("git");
        commit
            .arg("-C")
            .arg(&work)
            .arg("commit")
            .arg("-m")
            .arg(format!("edit {}", content.len()));
        git_env(&mut commit);
        must_succeed(commit, "git commit edit");

        let mut push = Command::new("git");
        push.arg("-C")
            .arg(&work)
            .arg("push")
            .arg("origin")
            .arg("main");
        git_env(&mut push);
        must_succeed(push, "git push edit");

        let out = Command::new("git")
            .arg("-C")
            .arg(bare)
            .arg("rev-parse")
            .arg("refs/heads/main")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn file_url(path: &Path) -> String {
        format!("file://{}", path.display())
    }

    fn build_manifest(
        id: &str,
        project_id: &str,
        source_url: &str,
        internal_bare_path: &Path,
        push_external: bool,
        sync_interval_secs: u64,
    ) -> RepoManifest {
        RepoManifest {
            id: id.to_string(),
            project_id: project_id.to_string(),
            slug: "test".to_string(),
            title: "test".to_string(),
            state: RepoState::Active,
            repo_role: RepoRole::Source,
            source: RepoSource {
                url: source_url.to_string(),
                provider: RepoProvider::Local,
                default_branch: "main".to_string(),
                protected_branches: vec![],
                pinned_head: None,
            },
            credential: RepoCredentialBinding {
                id: "cred:scheduler-test".to_string(),
                scope: CredentialScope::Push,
                trust_floor: AgentTrustLevel::ReadOnly,
            },
            mirror: RepoMirrorPolicy {
                internal_bare_path: internal_bare_path.to_path_buf(),
                direction: MirrorDirection::Bidirectional,
                sync_interval_secs,
                last_pulled_at: None,
                last_pushed_at: None,
            },
            checkout: RepoCheckoutPolicy {
                worktree_root: PathBuf::from("data/worktrees/test/"),
                agent_branch_prefix: "agent/".to_string(),
                max_concurrent_worktrees: 1,
                cleanup_on_goal_close: true,
            },
            agent_scopes: RepoAgentScopes {
                read: vec![],
                write: vec![],
                push_external,
                requires_operator_approval_for: vec![],
            },
            hooks: RepoHooks {
                on_attach: None,
                on_drift_detected: None,
                on_detach: None,
            },
            indexing: None,
            metadata: RepoMetadata {
                attached_at: "2026-04-17T00:00:00Z".to_string(),
                attached_by: "test".to_string(),
                notes: String::new(),
            },
            archeology_policy: None,
            body_markdown: String::new(),
        }
    }

    fn seed_vault(dir: &Path, credential_id: &str) -> Arc<GoalScopedVault> {
        let vault = GoalScopedVault::open(dir).expect("open vault");
        vault
            .put_scoped(
                None,
                CredentialRecord {
                    service: credential_id.to_string(),
                    username: "test".to_string(),
                    secret: String::new(),
                    totp_secret: None,
                },
            )
            .expect("seed credential");
        Arc::new(vault)
    }

    struct TestPushProvider;
    impl PushProvider for TestPushProvider {
        fn send(&self, _notification: &crate::push::PushNotification) -> anyhow::Result<()> {
            Ok(())
        }
    }

    fn test_deps(
        tmp: &TempDir,
    ) -> (
        SchedulerDeps,
        tokio::sync::mpsc::UnboundedReceiver<crate::MatrixOutboundMessage>,
        tokio::sync::mpsc::UnboundedReceiver<ConflictGoalRequest>,
    ) {
        let archive_root = tmp.path().join("archive");
        std::fs::create_dir_all(&archive_root).unwrap();
        let vault_dir = tmp.path().join("vault");
        let vault = seed_vault(&vault_dir, "cred:scheduler-test");
        let (mtx, mrx) = unbounded_channel::<crate::MatrixOutboundMessage>();
        let (ctx, crx) = unbounded_channel::<ConflictGoalRequest>();
        let registry = Arc::new(tokio::sync::Mutex::new(RepoRegistry::new()));
        let deps = SchedulerDeps {
            registry,
            approval_gate: Arc::new(std::sync::Mutex::new(ApprovalGate::new())),
            credential_vault: vault,
            push_provider: Arc::new(TestPushProvider) as Arc<dyn PushProvider>,
            matrix_outbound_tx: mtx,
            conflict_goal_tx: ctx,
            operator_room_id: "!ops:test".to_string(),
            project_goals_room: "!goals:test".to_string(),
            archive_root,
            agent_id: "scheduler@symbiotic.sh".to_string(),
            approval_ttl_secs: 60,
            approval_poll_interval_ms: 50,
        };
        (deps, mrx, crx)
    }

    // ── Tests ─────────────────────────────────────────────────────────────

    // 1
    #[tokio::test]
    async fn spawn_from_registry_creates_one_task_per_active_repo() {
        let tmp = TempDir::new().unwrap();
        let (deps, _mrx, _crx) = test_deps(&tmp);

        // Seed registry: 2 active + 1 detached. Sleep interval huge so tasks
        // don't actually do any work before we drop them.
        {
            let mut reg = deps.registry.lock().await;
            let bare_a = tmp.path().join("a.git");
            let bare_b = tmp.path().join("b.git");
            let bare_c = tmp.path().join("c.git");
            let m_a = build_manifest(
                "repo:a",
                "project:x",
                "file:///dev/null",
                &bare_a,
                false,
                3_600,
            );
            let m_b = build_manifest(
                "repo:b",
                "project:x",
                "file:///dev/null",
                &bare_b,
                false,
                3_600,
            );
            let mut m_c = build_manifest(
                "repo:c",
                "project:x",
                "file:///dev/null",
                &bare_c,
                false,
                3_600,
            );
            m_c.state = RepoState::Detached;
            reg.on_repo_attached(m_a).unwrap();
            reg.on_repo_attached(m_b).unwrap();
            // Insert the detached one and transition it.
            reg.on_repo_attached(m_c).unwrap();
            reg.on_repo_detached("repo:c").unwrap();
        }

        let (cancel_tx, cancel_rx) = watch::channel(false);
        let handles = spawn_from_registry(deps, cancel_rx).await;
        assert_eq!(handles.len(), 2, "two active repos → two tasks");

        cancel_tx.send(true).unwrap();
        for h in handles {
            let _ = tokio::time::timeout(Duration::from_secs(2), h).await;
        }
    }

    // 2
    #[test]
    fn decide_pulls_when_local_bare_not_initialized() {
        let tmp = TempDir::new().unwrap();
        let source = init_bare_with_commit(tmp.path(), "source", "hello\n");
        let local = tmp.path().join("local.git"); // not created
        let m = build_manifest("repo:t", "project:x", &file_url(&source), &local, false, 60);
        assert_eq!(decide(&m), LoopAction::Pull);
    }

    // 3
    #[test]
    fn decide_noop_when_local_equals_remote() {
        let tmp = TempDir::new().unwrap();
        let source = init_bare_with_commit(tmp.path(), "source", "hello\n");

        // Local bare = clone --bare of source → heads match.
        let local = tmp.path().join("local.git");
        let mut clone = Command::new("git");
        clone.arg("clone").arg("--bare").arg(&source).arg(&local);
        git_env(&mut clone);
        must_succeed(clone, "clone --bare");

        let m = build_manifest("repo:t", "project:x", &file_url(&source), &local, true, 60);
        assert_eq!(decide(&m), LoopAction::Noop);
    }

    // 4
    #[test]
    fn decide_pushes_when_local_ahead_and_push_external_enabled() {
        let tmp = TempDir::new().unwrap();
        let source = init_bare_with_commit(tmp.path(), "source", "v1\n");

        // Local = clone --bare of source, then add a commit to local.
        let local = tmp.path().join("local.git");
        let mut clone = Command::new("git");
        clone.arg("clone").arg("--bare").arg(&source).arg(&local);
        git_env(&mut clone);
        must_succeed(clone, "clone --bare");

        let _new = add_commit_to_bare(&local, &tmp, "v2\n");

        let m = build_manifest("repo:t", "project:x", &file_url(&source), &local, true, 60);
        assert_eq!(decide(&m), LoopAction::Push);

        // Same state but push_external disabled → Noop (local ahead of remote
        // but not permitted to push).
        let m_noop = build_manifest("repo:t", "project:x", &file_url(&source), &local, false, 60);
        assert_eq!(decide(&m_noop), LoopAction::Noop);
    }

    // 5
    #[test]
    fn decide_pulls_when_remote_ahead_of_local() {
        let tmp = TempDir::new().unwrap();
        let source = init_bare_with_commit(tmp.path(), "source", "v1\n");
        let local = tmp.path().join("local.git");
        let mut clone = Command::new("git");
        clone.arg("clone").arg("--bare").arg(&source).arg(&local);
        git_env(&mut clone);
        must_succeed(clone, "clone --bare");

        // Advance remote after local mirror is cloned.
        let _new_remote = add_commit_to_bare(&source, &tmp, "v2\n");

        let m = build_manifest("repo:t", "project:x", &file_url(&source), &local, true, 60);
        assert_eq!(decide(&m), LoopAction::Pull);
    }

    // 6
    #[test]
    fn decide_conflict_when_diverged() {
        let tmp = TempDir::new().unwrap();
        let source = init_bare_with_commit(tmp.path(), "source", "v1\n");

        let local = tmp.path().join("local.git");
        let mut clone = Command::new("git");
        clone.arg("clone").arg("--bare").arg(&source).arg(&local);
        git_env(&mut clone);
        must_succeed(clone, "clone --bare");

        // Advance BOTH — distinct commits → divergence.
        let _r = add_commit_to_bare(&source, &tmp, "remote-change\n");
        let _l = add_commit_to_bare(&local, &tmp, "local-change\n");

        let m = build_manifest("repo:t", "project:x", &file_url(&source), &local, true, 60);
        assert_eq!(decide(&m), LoopAction::Conflict);
    }

    // 7
    #[tokio::test]
    async fn conflict_tick_emits_archive_event_and_enqueues_goal() {
        let tmp = TempDir::new().unwrap();
        let (deps, _mrx, mut crx) = test_deps(&tmp);

        // Build diverged source + local.
        let source = init_bare_with_commit(tmp.path(), "source", "v1\n");
        let local = tmp.path().join("local.git");
        let mut clone = Command::new("git");
        clone.arg("clone").arg("--bare").arg(&source).arg(&local);
        git_env(&mut clone);
        must_succeed(clone, "clone --bare");
        let _r = add_commit_to_bare(&source, &tmp, "remote\n");
        let _l = add_commit_to_bare(&local, &tmp, "local\n");

        let manifest = build_manifest(
            "repo:conf",
            "project:x",
            &file_url(&source),
            &local,
            true,
            60,
        );

        let task_deps = TaskDeps::from_shared(&deps);
        run_one_tick(&manifest, &task_deps).await;

        // (a) archive event file exists
        let events_dir = deps
            .archive_root
            .join("operations")
            .join("projects")
            .join("x")
            .join("repos")
            .join("events");
        let files = std::fs::read_dir(&events_dir)
            .expect("events dir exists")
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| n.contains("repo-mirror-conflict-opened"))
                    .unwrap_or(false)
            })
            .collect::<Vec<_>>();
        assert_eq!(files.len(), 1, "one conflict event written: {files:?}");

        // (b) conflict-goal request arrived with scheduler sender
        let req = crx.try_recv().expect("conflict goal request enqueued");
        assert_eq!(req.sender, "scheduler@symbiotic.sh");
        assert!(
            req.description.contains("repo:conf"),
            "description mentions repo id: {}",
            req.description
        );
        assert_eq!(req.room_id, "!goals:test");
    }

    // 8
    #[tokio::test]
    async fn cancel_token_terminates_running_tasks() {
        let tmp = TempDir::new().unwrap();
        let (deps, _mrx, _crx) = test_deps(&tmp);

        // Seed one manifest; large sync interval so the loop immediately
        // parks in select!.
        {
            let mut reg = deps.registry.lock().await;
            let source = init_bare_with_commit(tmp.path(), "source", "v1\n");
            let local = tmp.path().join("local.git");
            let m = build_manifest(
                "repo:stop",
                "project:x",
                &file_url(&source),
                &local,
                false,
                3_600,
            );
            reg.on_repo_attached(m).unwrap();
        }

        let (cancel_tx, cancel_rx) = watch::channel(false);
        let handles = spawn_from_registry(deps, cancel_rx).await;
        assert_eq!(handles.len(), 1);

        cancel_tx.send(true).unwrap();

        for h in handles {
            let res = tokio::time::timeout(Duration::from_secs(2), h).await;
            assert!(
                res.is_ok(),
                "task should exit within 2s of cancellation; timed out"
            );
        }
    }

    // 9 — sanity: the `DaemonMatrixPoster` we hand to `do_push` is usable
    // outside its normal runtime wiring (proves the scheduler's poster clone
    // path is shape-correct).
    #[tokio::test]
    async fn daemon_matrix_poster_from_scheduler_deps_sends_to_outbound() {
        let tmp = TempDir::new().unwrap();
        let (deps, mut mrx, _crx) = test_deps(&tmp);
        let poster = DaemonMatrixPoster::new(deps.matrix_outbound_tx.clone());
        poster
            .post_text("!room:test", "hello from scheduler", 1_700_000_000)
            .await
            .expect("post ok");
        let (room_id, _env) = mrx.recv().await.expect("outbound received");
        assert_eq!(room_id, "!room:test");
    }
}
