//! Pull half of the mirror loop — idempotent `git fetch` from a repo manifest's
//! source URL into the daemon's durable bare mirror at
//! `manifest.mirror.internal_bare_path`. No credential handling lives in this
//! chunk; source URLs must be public, `file://`, or rely on host-side SSH
//! configuration. See `docs/design/repo-manifest.md` §Mirror Loop and
//! `tasks/126-repo-manifest/04-daemon-mirror.md` for scope.
//!
//! The push half (`git push source`), the credential-sandbox mediation, the
//! background scheduler, the role gate, and the lifecycle events all land in
//! §05/§06.

use std::path::Path;
use std::process::{Command, Output};

use symbiotic_control_plane::RepoManifest;
use thiserror::Error;

/// Error surface for a single pull-once invocation.
#[derive(Debug, Error)]
pub enum RepoMirrorError {
    #[error("git command failed ({op}): status {status}, stderr: {stderr}")]
    GitCommand {
        op: &'static str,
        status: String,
        stderr: String,
    },
    #[error("IO error ({ctx}): {source}")]
    Io {
        ctx: String,
        #[source]
        source: std::io::Error,
    },
    #[error("push_external disabled on manifest {id}")]
    PushExternalDisabled { id: String },
    #[error("push credential missing: {0}")]
    PushCredentialMissing(String),
    #[error("push credential scope denied: {0}")]
    PushCredentialScopeDenied(String),
    #[error("approval denied: {reason:?}")]
    ApprovalDenied { reason: Option<String> },
    #[error("approval expired (ticket {ticket_id})")]
    ApprovalExpired { ticket_id: String },
}

/// Context passed by callers of `mirror_pull_once` that want a
/// `repo_mirror_pull_completed` event written to the knowledge-base archive
/// after a successful pull. When `None` is passed, no event is emitted.
///
/// `archive_root` is the root of the knowledge-base (the directory that
/// contains `operations/`). `project_id` is the `project:{slug}` id.
/// `observed_at` is Unix seconds.
pub struct RepoMirrorArchiveContext<'a> {
    pub archive_root: &'a Path,
    pub project_id: &'a str,
    pub observed_at: i64,
}

/// Report from a single `mirror_pull_once` invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MirrorPullReport {
    /// True iff the bare repo at `internal_bare_path` had to be initialized
    /// this call.
    pub initialized: bool,
    /// True iff at least one ref advanced. Naive check: HEAD of
    /// `default_branch` differs from its pre-fetch value. On first init
    /// followed by a successful fetch with non-empty refs, always true.
    pub refs_advanced: bool,
    /// The post-fetch HEAD commit SHA of `default_branch`, if resolvable;
    /// `None` when the source has no commits on that branch yet.
    pub head_after: Option<String>,
}

/// Run exactly one pull from `manifest.source.url` into
/// `manifest.mirror.internal_bare_path`.
///
/// Behavior:
/// 1. Ensure parent directory exists.
/// 2. If `internal_bare_path` is not a bare repo, `git init --bare` and
///    attach `origin = manifest.source.url`. Remote URL is kept in sync via
///    `git remote set-url origin ...` regardless.
/// 3. Capture pre-fetch HEAD of `default_branch` via `git rev-parse`.
/// 4. `git fetch origin '+refs/heads/*:refs/heads/*' '+refs/tags/*:refs/tags/*' --prune`.
/// 5. Capture post-fetch HEAD of `default_branch`.
/// 6. Report initialization, ref-advancement, and post-fetch HEAD.
///
/// Credential-free by design in this chunk: `source.url` must be a public URL,
/// a `file://` URL (tests), or an SSH URL resolvable by host-side SSH config.
/// §05 replaces this with credential-sandbox mediation.
pub fn mirror_pull_once(
    manifest: &RepoManifest,
    archive_context: Option<&RepoMirrorArchiveContext>,
) -> Result<MirrorPullReport, RepoMirrorError> {
    let bare_path = manifest.mirror.internal_bare_path.as_path();
    let source_url = manifest.source.url.as_str();
    let default_branch = manifest.source.default_branch.as_str();

    // 1. Ensure parent directory exists.
    if let Some(parent) = bare_path.parent() {
        if !parent.as_os_str().is_empty() && !parent.exists() {
            std::fs::create_dir_all(parent).map_err(|source| RepoMirrorError::Io {
                ctx: format!("create_dir_all({})", parent.display()),
                source,
            })?;
        }
    }

    // 2. Initialize if needed.
    let initialized = !is_bare_repo(bare_path);
    if initialized {
        // `git init --bare <path>` works even if the directory doesn't exist yet.
        // `--initial-branch` is explicit so HEAD points at refs/heads/{default_branch}
        // from the start. Without it, systems with `init.defaultBranch` unset
        // (including the Ubuntu Actions runner images) fall back to "master",
        // leaving the bare's HEAD as a dangling symref once the fetch populates
        // a different default branch. That dangling symref breaks downstream
        // clones — they land with a detached HEAD and subsequent `git push
        // origin main` fails with "src refspec main does not match any".
        let mut init_cmd = Command::new("git");
        init_cmd
            .arg("init")
            .arg("--bare")
            .arg(format!("--initial-branch={default_branch}"))
            .arg(bare_path);
        git_output(&mut init_cmd, "init")?;

        // `git remote add origin <url>` — if another `origin` exists from a
        // partial prior init, the command fails; we always follow up with
        // `remote set-url` below to keep the origin URL canonical.
        let mut add_cmd = Command::new("git");
        add_cmd
            .arg("-C")
            .arg(bare_path)
            .arg("remote")
            .arg("add")
            .arg("origin")
            .arg(source_url);
        // Ignore errors from `remote add`; `remote set-url` handles the
        // already-exists case cleanly.
        let _ = add_cmd.output();
    }

    // Unconditionally keep origin URL in sync with the manifest.
    let mut set_url_cmd = Command::new("git");
    set_url_cmd
        .arg("-C")
        .arg(bare_path)
        .arg("remote")
        .arg("set-url")
        .arg("origin")
        .arg(source_url);
    git_output(&mut set_url_cmd, "remote set-url")?;

    // 3. Pre-fetch HEAD of `default_branch`, if resolvable.
    let head_before = rev_parse_branch(bare_path, default_branch)?;

    // 4. Fetch.
    let mut fetch_cmd = Command::new("git");
    fetch_cmd
        .arg("-C")
        .arg(bare_path)
        .arg("fetch")
        .arg("origin")
        .arg("+refs/heads/*:refs/heads/*")
        .arg("+refs/tags/*:refs/tags/*")
        .arg("--prune");
    git_output(&mut fetch_cmd, "fetch")?;

    // 5. Post-fetch HEAD of `default_branch`.
    let head_after = rev_parse_branch(bare_path, default_branch)?;

    // 6. Report.
    let refs_advanced = head_after != head_before || (initialized && head_after.is_some());

    let report = MirrorPullReport {
        initialized,
        refs_advanced,
        head_after,
    };

    // 7. Emit `repo_mirror_pull_completed` event if a context was provided.
    //    Event-write failure must NOT fail the pull — log and continue.
    if let Some(ctx) = archive_context {
        let head_after_yaml = match &report.head_after {
            Some(sha) => format!("\"{}\"", sha),
            None => "null".to_string(),
        };
        let head_after_display = report
            .head_after
            .as_deref()
            .map(|sha| format!("`{sha}`"))
            .unwrap_or_else(|| "(none)".to_string());
        let payload = format!(
            "head_after: {}\nrefs_advanced: {}\ninitialized: {}\n",
            head_after_yaml, report.refs_advanced, report.initialized
        );
        let body = format!(
            "Mirror pull completed for `repo:{slug}`.\nInitialized: {initialized}. Refs advanced: {refs_advanced}. Head now: {head}.\n",
            slug = manifest.slug,
            initialized = report.initialized,
            refs_advanced = report.refs_advanced,
            head = head_after_display,
        );
        if let Err(e) = crate::repo_events::append_repo_event_archive(
            ctx.archive_root,
            ctx.project_id,
            &manifest.id,
            "repo_mirror_pull_completed",
            ctx.observed_at,
            &manifest.slug,
            &payload,
            &body,
        ) {
            tracing::warn!("failed to emit repo_mirror_pull_completed: {e}");
        }
    }

    Ok(report)
}

/// Report from a single `mirror_push_once` invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MirrorPushReport {
    /// True iff `git push` succeeded (non-zero exit → `Err`, so when this
    /// value is constructed it is always `true` on the success path).
    pub pushed: bool,
    /// Remote HEAD of the target branch pre-push. Intentionally `None` in
    /// this chunk; §08 scheduler can populate via `git ls-remote`.
    pub head_before: Option<String>,
    /// Local bare HEAD of `branch` post-push (i.e. the SHA we just pushed).
    pub head_after: Option<String>,
}

/// Derive the credential materialization kind from the source URL.
///
/// Loose heuristic for T126:
/// - `https://` / `http://` → `HttpsToken`
/// - Everything else (`ssh://`, `git@host:repo.git`, `file://`, local
///   paths) → `SshPrivateKey`
///
/// For URLs that do not need auth at all (e.g. `file://` in tests), the
/// materialized env vars are harmless — `GIT_SSH_COMMAND` is only invoked
/// when git actually uses SSH transport.
fn derive_credential_kind(url: &str) -> credential_gateway::GitCredentialKind {
    if url.starts_with("https://") || url.starts_with("http://") {
        credential_gateway::GitCredentialKind::HttpsToken
    } else {
        credential_gateway::GitCredentialKind::SshPrivateKey
    }
}

/// Run exactly one push from `manifest.mirror.internal_bare_path` to
/// `manifest.source.url` using a scope-guarded credential session.
///
/// Gated by `manifest.agent_scopes.push_external` — returns
/// `PushExternalDisabled` when the manifest disallows external pushes.
///
/// Per-agent author/committer identity is threaded into the subprocess env
/// (D1b). For forward-only pushes these env vars are not consulted; they
/// are only used if git needs to synthesize a commit during the push (rare
/// merge-on-push flows).
///
/// `archive_context: Some(..)` triggers emission of
/// `repo_mirror_push_completed` via `repo_events::append_repo_event_archive`
/// on the success path. Event-write failure is logged via
/// `tracing::warn!` and does NOT fail the push (matches pull invariant).
///
/// `goal_scope` is forwarded to the credential vault — `None` resolves the
/// credential from the global namespace.
pub fn mirror_push_once(
    manifest: &RepoManifest,
    agent_id: &str,
    vault: &credential_gateway::GoalScopedVault,
    archive_context: Option<&RepoMirrorArchiveContext>,
    branch: &str,
    goal_scope: Option<&str>,
) -> Result<MirrorPushReport, RepoMirrorError> {
    // 1. Gate check.
    if !manifest.agent_scopes.push_external {
        return Err(RepoMirrorError::PushExternalDisabled {
            id: manifest.id.clone(),
        });
    }

    // 2. Read local head (what we are about to push).
    let bare_path = manifest.mirror.internal_bare_path.as_path();
    let ref_name = format!("refs/heads/{branch}");
    let rev_parse_output = Command::new("git")
        .arg("-C")
        .arg(bare_path)
        .arg("rev-parse")
        .arg(&ref_name)
        .output()
        .map_err(|source| RepoMirrorError::Io {
            ctx: format!("spawn git rev-parse {ref_name}"),
            source,
        })?;
    if !rev_parse_output.status.success() {
        return Err(RepoMirrorError::GitCommand {
            op: "rev-parse",
            status: rev_parse_output.status.to_string(),
            stderr: String::from_utf8_lossy(&rev_parse_output.stderr).into_owned(),
        });
    }
    let head_after = String::from_utf8_lossy(&rev_parse_output.stdout)
        .trim()
        .to_string();

    // 3. head_before intentionally None (see spec §62 / field doc).

    // 4. Derive credential kind from source URL.
    let kind = derive_credential_kind(manifest.source.url.as_str());

    // 5. Push with credential session.
    let push_result = credential_gateway::with_git_push_session(
        vault,
        &manifest.credential.id,
        kind,
        goal_scope,
        |env| {
            let mut cmd = Command::new("git");
            cmd.arg("-C")
                .arg(&manifest.mirror.internal_bare_path)
                .arg("push")
                .arg(&manifest.source.url)
                .arg(format!("refs/heads/{branch}:refs/heads/{branch}"));
            env.apply(&mut cmd);
            // Per-agent author identity (D1b).
            cmd.env("GIT_AUTHOR_NAME", format!("Symbiotic Agent {agent_id}"))
                .env("GIT_AUTHOR_EMAIL", format!("agent-{agent_id}@symbiotic.sh"))
                .env("GIT_COMMITTER_NAME", format!("Symbiotic Agent {agent_id}"))
                .env(
                    "GIT_COMMITTER_EMAIL",
                    format!("agent-{agent_id}@symbiotic.sh"),
                );
            let output = cmd
                .output()
                .map_err(credential_gateway::GitPushSessionError::Io)?;
            if !output.status.success() {
                return Err(credential_gateway::GitPushSessionError::Subprocess(
                    String::from_utf8_lossy(&output.stderr).into_owned(),
                ));
            }
            Ok(())
        },
    );

    // 6. Map credential-session errors to RepoMirrorError.
    if let Err(err) = push_result {
        return Err(match err {
            credential_gateway::GitPushSessionError::CredentialNotFound(s) => {
                RepoMirrorError::PushCredentialMissing(s)
            }
            credential_gateway::GitPushSessionError::ScopeDenied(s) => {
                RepoMirrorError::PushCredentialScopeDenied(s)
            }
            credential_gateway::GitPushSessionError::Subprocess(stderr) => {
                RepoMirrorError::GitCommand {
                    op: "push",
                    status: "nonzero".to_string(),
                    stderr,
                }
            }
            credential_gateway::GitPushSessionError::Io(io) => RepoMirrorError::Io {
                ctx: "push".to_string(),
                source: io,
            },
        });
    }

    // 7. Emit `repo_mirror_push_completed` event if a context was provided.
    //    Event-write failure must NOT fail the push.
    if let Some(ctx) = archive_context {
        let payload = format!(
            "branch: \"{branch}\"\nhead_after: \"{head_after}\"\nagent_id: \"{agent_id}\"\n"
        );
        let body = format!(
            "Mirror push completed for `repo:{slug}` branch `{branch}` by agent `{agent_id}`. Head pushed: {head_after}.\n",
            slug = manifest.slug,
        );
        if let Err(e) = crate::repo_events::append_repo_event_archive(
            ctx.archive_root,
            ctx.project_id,
            &manifest.id,
            "repo_mirror_push_completed",
            ctx.observed_at,
            &manifest.slug,
            &payload,
            &body,
        ) {
            tracing::warn!("failed to emit repo_mirror_push_completed: {e}");
        }
    }

    Ok(MirrorPushReport {
        pushed: true,
        head_before: None,
        head_after: Some(head_after),
    })
}

// ── Approval-gated push wrapper (§07b.ii-b) ────────────────────────────

/// Async wrapper around `mirror_push_once` that gates the push through the
/// operator approval state machine when the manifest's
/// `agent_scopes.requires_operator_approval_for` includes `"push_external"`.
///
/// Flow when approval is required:
/// 1. Compute the approval context (commit range, diff stats, top commit
///    message, protected-branch flag) from the local bare mirror.
/// 2. Open a ticket on `approval_gate`.
/// 3. Emit `repo_external_push_approval_requested` archive event.
/// 4. Post the human-readable body to `operator_room_id` via the
///    `matrix_poster` trait impl (production: stub; tests: mock).
/// 5. Fan out a single broadcast `PushNotification` via `push_provider`.
///    Per-device fan-out lives in `push_dispatcher.rs` — a follow-up chunk
///    can thread that path once the scheduler (§08) wires this wrapper.
/// 6. Poll the gate on `poll_interval_ms` cadence until either the ticket
///    transitions away from `Pending`, or the deadline (`now + ttl_secs`)
///    passes and `expire_stale` marks it `Expired`.
/// 7. Emit the matching outcome event (`_approved` / `_denied` / `_expired`).
/// 8. On approval, delegate to the sync `mirror_push_once`. Otherwise return
///    the typed approval error.
///
/// When approval is NOT required (no `push_external` entry in the manifest's
/// `requires_operator_approval_for`), this delegates straight to
/// `mirror_push_once` with no side effects.
#[allow(clippy::too_many_arguments)]
pub async fn mirror_push_with_approval(
    manifest: &RepoManifest,
    agent_id: &str,
    vault: &credential_gateway::GoalScopedVault,
    approval_gate: &std::sync::Arc<std::sync::Mutex<crate::approval_gate::ApprovalGate>>,
    matrix_poster: &dyn crate::matrix_poster::MatrixPoster,
    push_provider: &dyn crate::push::PushProvider,
    operator_room_id: &str,
    archive_context: Option<&RepoMirrorArchiveContext<'_>>,
    branch: &str,
    goal_id: Option<&str>,
    goal_scope: Option<&str>,
    ttl_secs: u64,
    poll_interval_ms: u64,
) -> Result<MirrorPushReport, RepoMirrorError> {
    // 1. Early exit when approval is not required.
    let requires_approval = manifest
        .agent_scopes
        .requires_operator_approval_for
        .iter()
        .any(|op| op == "push_external");
    if !requires_approval {
        return mirror_push_once(
            manifest,
            agent_id,
            vault,
            archive_context,
            branch,
            goal_scope,
        );
    }

    // 2. Derive approval-context metadata from the local bare mirror. These
    //    git invocations are best-effort: failures fall back to conservative
    //    defaults rather than blocking the approval flow.
    let bare_path = manifest.mirror.internal_bare_path.as_path();
    let ref_name = format!("refs/heads/{branch}");

    // head_after: required. If we can't resolve the head we can't ask for
    // approval on a meaningful push, so bubble the git error.
    let rev_parse_out = Command::new("git")
        .arg("-C")
        .arg(bare_path)
        .arg("rev-parse")
        .arg(&ref_name)
        .output()
        .map_err(|source| RepoMirrorError::Io {
            ctx: format!("spawn git rev-parse {ref_name}"),
            source,
        })?;
    if !rev_parse_out.status.success() {
        return Err(RepoMirrorError::GitCommand {
            op: "rev-parse",
            status: rev_parse_out.status.to_string(),
            stderr: String::from_utf8_lossy(&rev_parse_out.stderr).into_owned(),
        });
    }
    let head_after = String::from_utf8_lossy(&rev_parse_out.stdout)
        .trim()
        .to_string();

    // commit_count: best-effort rev-list count; defaults to 0 on failure.
    let commit_count: u32 = Command::new("git")
        .arg("-C")
        .arg(bare_path)
        .arg("rev-list")
        .arg("--count")
        .arg(&ref_name)
        .output()
        .ok()
        .filter(|out| out.status.success())
        .and_then(|out| {
            String::from_utf8_lossy(&out.stdout)
                .trim()
                .parse::<u32>()
                .ok()
        })
        .unwrap_or(0);

    // top_commit_message: best-effort subject line; defaults to "(unknown)".
    let top_commit_message = Command::new("git")
        .arg("-C")
        .arg(bare_path)
        .arg("log")
        .arg("-1")
        .arg("--format=%s")
        .arg(&ref_name)
        .output()
        .ok()
        .and_then(|out| {
            if out.status.success() {
                Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
            } else {
                None
            }
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "(unknown)".to_string());

    // diff_stats: best-effort shortstat over the last `commit_count` commits
    // (HEAD~N..HEAD). On any failure — including first-commit / missing
    // parent cases — default to zeros.
    let diff_stats = if commit_count > 0 {
        let range = format!("HEAD~{commit_count}..HEAD");
        let out = Command::new("git")
            .arg("-C")
            .arg(bare_path)
            .arg("diff")
            .arg("--shortstat")
            .arg(&range)
            .output()
            .ok();
        match out {
            Some(o) if o.status.success() => parse_shortstat(&String::from_utf8_lossy(&o.stdout)),
            _ => crate::approval_gate::DiffStats {
                files_changed: 0,
                insertions: 0,
                deletions: 0,
            },
        }
    } else {
        crate::approval_gate::DiffStats {
            files_changed: 0,
            insertions: 0,
            deletions: 0,
        }
    };

    let is_protected = manifest
        .source
        .protected_branches
        .iter()
        .any(|b| b == branch);
    let goal_display = goal_id.unwrap_or("(adhoc)");
    let explanation = format!(
        "Agent {agent_id} wants to push {commit_count} commits to {url}:{branch} from goal {goal_display}.",
        url = manifest.source.url,
    );

    let context = crate::approval_gate::ApprovalContext {
        operation: crate::approval_gate::ApprovalOperation::PushExternal,
        explanation: explanation.clone(),
        repo_id: manifest.id.clone(),
        remote_url: manifest.source.url.clone(),
        local_bare_path: manifest.mirror.internal_bare_path.display().to_string(),
        branch: branch.to_string(),
        is_protected_branch: is_protected,
        agent_id: agent_id.to_string(),
        goal_id: goal_id.map(str::to_string),
        project_id: manifest.project_id.clone(),
        commit_range: crate::approval_gate::CommitRange {
            from_sha: "0000000000000000000000000000000000000000".to_string(),
            to_sha: head_after.clone(),
            commit_count,
        },
        diff_stats: diff_stats.clone(),
        top_commit_message: top_commit_message.clone(),
        archeology_detail: None,
    };

    // 3. Open ticket.
    let now = symbiotic_core::now_unix();
    let ticket = {
        let mut gate = approval_gate.lock().expect("approval_gate mutex poisoned");
        gate.open_ticket(context.clone(), now, ttl_secs)
    };

    // 4. Emit `repo_external_push_approval_requested` archive event.
    if let Some(ctx) = archive_context {
        let payload = format!(
            "ticket_id: \"{ticket_id}\"\nagent_id: \"{agent_id}\"\nbranch: \"{branch}\"\ncommit_count: {commit_count}\nis_protected_branch: {is_protected}\ntop_commit_message: {top_commit_message:?}\nfiles_changed: {files}\ninsertions: {ins}\ndeletions: {del}\n",
            ticket_id = ticket.ticket_id,
            files = diff_stats.files_changed,
            ins = diff_stats.insertions,
            del = diff_stats.deletions,
        );
        let body = format!(
            "Agent `{agent_id}` requested approval to push `{branch}` ({commit_count} commits) to `{url}`. Ticket `{ticket_id}`.\n",
            url = manifest.source.url,
            ticket_id = ticket.ticket_id,
        );
        if let Err(e) = crate::repo_events::append_repo_event_archive(
            ctx.archive_root,
            ctx.project_id,
            &manifest.id,
            "repo_external_push_approval_requested",
            ctx.observed_at,
            &manifest.slug,
            &payload,
            &body,
        ) {
            tracing::warn!("failed to emit repo_external_push_approval_requested: {e}");
        }
    }

    // 5. Post matrix message.
    let protected_flag = if is_protected { " (protected)" } else { "" };
    let matrix_body = format!(
        "🔐 Push approval requested\n\nAgent {agent_id} wants to push to {repo_id}:\n→ {branch}{protected_flag}\n→ {commit_count} commits, +{ins}/-{del} across {files} files\n→ Top: \"{top}\"\nGoal: {goal}\n\nReply:\n  approve {ticket_id}\n  deny {ticket_id} [reason]\n  inspect {ticket_id}    (full diff)\n",
        repo_id = manifest.id,
        files = diff_stats.files_changed,
        ins = diff_stats.insertions,
        del = diff_stats.deletions,
        top = top_commit_message,
        goal = goal_id.unwrap_or("(no goal)"),
        ticket_id = ticket.ticket_id,
    );
    matrix_poster
        .post_text(operator_room_id, &matrix_body, now)
        .await
        .map_err(|source| RepoMirrorError::Io {
            ctx: "matrix_poster.post_text".to_string(),
            source: std::io::Error::other(source.to_string()),
        })?;

    // 6. Send push notification.
    let notif = crate::push::PushNotification {
        notification_id: format!("approval-{}", ticket.ticket_id),
        device_id: "*".to_string(),
        token_hash: String::new(),
        encrypted_token: String::new(),
        platform: String::new(),
        priority: crate::push::PushPriority::High.as_str().to_string(),
        title: format!("Push approval — {}", manifest.slug),
        body: format!(
            "{agent_id} → {branch}: {commit_count} commits, +{ins}/-{del}",
            ins = diff_stats.insertions,
            del = diff_stats.deletions,
        ),
        rid: ticket.ticket_id.clone(),
        event_type: "repo_external_push_approval_requested".to_string(),
        event_status: "pending".to_string(),
        ts: now,
        thread_id: Some(manifest.project_id.clone()),
        badge: None,
    };
    if let Err(e) = push_provider.send(&notif) {
        // Push delivery failure is not fatal to the approval flow — the
        // matrix message already went out and the ticket lives in the gate.
        tracing::warn!("push_provider.send failed for approval ticket: {e}");
    }

    // 7. Poll the gate until the ticket is no longer pending.
    let outcome = {
        let deadline = now + ttl_secs;
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(poll_interval_ms)).await;
            let state = {
                let mut gate = approval_gate.lock().expect("approval_gate mutex poisoned");
                let now_check = symbiotic_core::now_unix();
                gate.expire_stale(now_check);
                let state = gate
                    .get(&ticket.ticket_id)
                    .map(|t| t.state.clone())
                    .ok_or_else(|| RepoMirrorError::Io {
                        ctx: "approval gate ticket vanished".to_string(),
                        source: std::io::Error::other("gate corruption"),
                    })?;
                // If still pending but deadline has passed, force-expire the
                // ticket so the next iteration observes Expired. `expire_stale`
                // only flips pending tickets whose `requested_at + ttl_secs <=
                // now_check`; we mirror that contract with a direct transition.
                if matches!(state, crate::approval_gate::ApprovalState::Pending)
                    && now_check > deadline
                {
                    // Force the underlying ticket to Expired via expire_stale
                    // after nudging its clock forward.
                    gate.expire_stale(now_check);
                }
                state
            };
            match state {
                crate::approval_gate::ApprovalState::Pending => continue,
                other => break other,
            }
        }
    };

    // 8. Emit matching outcome event + return.
    match outcome {
        crate::approval_gate::ApprovalState::Approved { .. } => {
            if let Some(ctx) = archive_context {
                let payload = format!(
                    "ticket_id: \"{ticket_id}\"\nagent_id: \"{agent_id}\"\nbranch: \"{branch}\"\n",
                    ticket_id = ticket.ticket_id,
                );
                let body = format!(
                    "Approval granted for push ticket `{ticket_id}`.\n",
                    ticket_id = ticket.ticket_id,
                );
                if let Err(e) = crate::repo_events::append_repo_event_archive(
                    ctx.archive_root,
                    ctx.project_id,
                    &manifest.id,
                    "repo_external_push_approval_approved",
                    ctx.observed_at,
                    &manifest.slug,
                    &payload,
                    &body,
                ) {
                    tracing::warn!("failed to emit repo_external_push_approval_approved: {e}");
                }
            }
            mirror_push_once(
                manifest,
                agent_id,
                vault,
                archive_context,
                branch,
                goal_scope,
            )
        }
        crate::approval_gate::ApprovalState::Denied { reason, .. } => {
            if let Some(ctx) = archive_context {
                let payload = format!(
                    "ticket_id: \"{ticket_id}\"\nagent_id: \"{agent_id}\"\nbranch: \"{branch}\"\nreason: {reason:?}\n",
                    ticket_id = ticket.ticket_id,
                );
                let body = format!(
                    "Approval denied for push ticket `{ticket_id}`.\n",
                    ticket_id = ticket.ticket_id,
                );
                if let Err(e) = crate::repo_events::append_repo_event_archive(
                    ctx.archive_root,
                    ctx.project_id,
                    &manifest.id,
                    "repo_external_push_approval_denied",
                    ctx.observed_at,
                    &manifest.slug,
                    &payload,
                    &body,
                ) {
                    tracing::warn!("failed to emit repo_external_push_approval_denied: {e}");
                }
            }
            Err(RepoMirrorError::ApprovalDenied { reason })
        }
        crate::approval_gate::ApprovalState::Expired => {
            if let Some(ctx) = archive_context {
                let payload = format!(
                    "ticket_id: \"{ticket_id}\"\nagent_id: \"{agent_id}\"\nbranch: \"{branch}\"\n",
                    ticket_id = ticket.ticket_id,
                );
                let body = format!(
                    "Approval expired for push ticket `{ticket_id}` (ttl {ttl_secs}s).\n",
                    ticket_id = ticket.ticket_id,
                );
                if let Err(e) = crate::repo_events::append_repo_event_archive(
                    ctx.archive_root,
                    ctx.project_id,
                    &manifest.id,
                    "repo_external_push_approval_expired",
                    ctx.observed_at,
                    &manifest.slug,
                    &payload,
                    &body,
                ) {
                    tracing::warn!("failed to emit repo_external_push_approval_expired: {e}");
                }
            }
            Err(RepoMirrorError::ApprovalExpired {
                ticket_id: ticket.ticket_id.clone(),
            })
        }
        crate::approval_gate::ApprovalState::Pending => {
            // The poll loop only breaks on non-pending states, so this arm
            // should be unreachable. Map to an Io error to be safe.
            Err(RepoMirrorError::Io {
                ctx: "approval poll loop exited on Pending".to_string(),
                source: std::io::Error::other("unreachable"),
            })
        }
    }
}

/// Parse a `git diff --shortstat` line of the form
/// `"5 files changed, 120 insertions(+), 30 deletions(-)"`. Returns zero
/// counts on parse failure — this is best-effort approval-context metadata,
/// not a correctness-critical path.
fn parse_shortstat(raw: &str) -> crate::approval_gate::DiffStats {
    let mut files_changed: u32 = 0;
    let mut insertions: u32 = 0;
    let mut deletions: u32 = 0;
    for segment in raw.split(',') {
        let trimmed = segment.trim();
        // Each segment is "<number> <word(s)>". Grab the first token as a u32.
        if let Some((num_tok, rest)) = trimmed.split_once(' ') {
            let Ok(n) = num_tok.parse::<u32>() else {
                continue;
            };
            if rest.contains("file") {
                files_changed = n;
            } else if rest.contains("insertion") {
                insertions = n;
            } else if rest.contains("deletion") {
                deletions = n;
            }
        }
    }
    crate::approval_gate::DiffStats {
        files_changed,
        insertions,
        deletions,
    }
}

// ── Helpers ────────────────────────────────────────────────────────────

/// Fast structural check that `path` is an already-initialized git bare repo.
fn is_bare_repo(path: &Path) -> bool {
    path.join("HEAD").exists() && path.join("objects").is_dir()
}

/// Run a `git rev-parse refs/heads/{branch}` in a bare repo. Returns `Ok(None)`
/// when the ref is absent (e.g., brand-new mirror with no commits yet).
fn rev_parse_branch(bare_path: &Path, branch: &str) -> Result<Option<String>, RepoMirrorError> {
    let ref_name = format!("refs/heads/{branch}");
    let output = Command::new("git")
        .arg("-C")
        .arg(bare_path)
        .arg("rev-parse")
        .arg(&ref_name)
        .output()
        .map_err(|source| RepoMirrorError::Io {
            ctx: format!("spawn git rev-parse {ref_name}"),
            source,
        })?;
    if !output.status.success() {
        // Missing ref is not an error for our purposes.
        return Ok(None);
    }
    let sha = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if sha.is_empty() {
        Ok(None)
    } else {
        Ok(Some(sha))
    }
}

/// Run `cmd` and return `Output` iff `status.success()`; otherwise map to
/// `RepoMirrorError::GitCommand` with captured stderr.
fn git_output(cmd: &mut Command, op: &'static str) -> Result<Output, RepoMirrorError> {
    let output = cmd.output().map_err(|source| RepoMirrorError::Io {
        ctx: format!("spawn git {op}"),
        source,
    })?;
    if !output.status.success() {
        return Err(RepoMirrorError::GitCommand {
            op,
            status: output.status.to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }
    Ok(output)
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use symbiotic_control_plane::{
        CredentialScope, MirrorDirection, RepoAgentScopes, RepoCheckoutPolicy,
        RepoCredentialBinding, RepoHooks, RepoManifest, RepoMetadata, RepoMirrorPolicy,
        RepoProvider, RepoRole, RepoSource, RepoState,
    };
    use symbiotic_trust::AgentTrustLevel;
    use tempfile::TempDir;

    /// Construct a minimal, valid in-memory `RepoManifest` pointing at
    /// `source_url` with a bare mirror at `bare_path`. `default_branch` is
    /// "main".
    fn build_manifest(source_url: &str, bare_path: &Path) -> RepoManifest {
        RepoManifest {
            id: "repo:test".to_string(),
            project_id: "project:test".to_string(),
            slug: "test".to_string(),
            title: "test".to_string(),
            state: RepoState::Active,
            repo_role: RepoRole::Source,
            source: RepoSource {
                url: source_url.to_string(),
                provider: RepoProvider::Local,
                default_branch: "main".to_string(),
                protected_branches: vec!["main".to_string()],
                pinned_head: None,
            },
            credential: RepoCredentialBinding {
                id: "credential:test".to_string(),
                scope: CredentialScope::Read,
                trust_floor: AgentTrustLevel::ReadOnly,
            },
            mirror: RepoMirrorPolicy {
                internal_bare_path: bare_path.to_path_buf(),
                direction: MirrorDirection::PullOnly,
                sync_interval_secs: 300,
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
                push_external: false,
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

    /// Run a git command under a set of test-safe env vars so commits work
    /// without a real `~/.gitconfig`.
    fn git_cmd(dir: &Path) -> Command {
        let mut cmd = Command::new("git");
        cmd.current_dir(dir)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null");
        cmd
    }

    /// Run the command, panic with stderr on non-zero exit. Used only in test
    /// setup.
    fn must_succeed(mut cmd: Command, label: &str) -> Output {
        let out = cmd
            .output()
            .unwrap_or_else(|e| panic!("{label} spawn: {e}"));
        assert!(
            out.status.success(),
            "{label} failed: status={} stderr={}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        );
        out
    }

    /// Initialize a non-bare source repo at `dir` with a commit adding a single
    /// file named `file_name` with `file_content`. Returns its HEAD SHA.
    fn init_source_repo(dir: &Path, file_name: &str, file_content: &str) -> String {
        let mut init = git_cmd(dir);
        init.arg("init").arg("--initial-branch=main").arg(".");
        must_succeed(init, "git init source");

        std::fs::write(dir.join(file_name), file_content).unwrap();

        let mut add = git_cmd(dir);
        add.arg("add").arg(file_name);
        must_succeed(add, "git add");

        let mut commit = git_cmd(dir);
        commit.arg("commit").arg("-m").arg("initial");
        must_succeed(commit, "git commit");

        head_sha(dir)
    }

    /// Append a second commit to the source repo and return the new HEAD SHA.
    fn add_commit(dir: &Path, file_name: &str, file_content: &str) -> String {
        std::fs::write(dir.join(file_name), file_content).unwrap();
        let mut add = git_cmd(dir);
        add.arg("add").arg(file_name);
        must_succeed(add, "git add (second)");
        let mut commit = git_cmd(dir);
        commit.arg("commit").arg("-m").arg("second");
        must_succeed(commit, "git commit (second)");
        head_sha(dir)
    }

    fn head_sha(dir: &Path) -> String {
        let out = git_cmd(dir)
            .arg("rev-parse")
            .arg("HEAD")
            .output()
            .expect("spawn rev-parse");
        assert!(out.status.success(), "rev-parse HEAD failed");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn file_url(path: &Path) -> String {
        format!("file://{}", path.display())
    }

    // 1
    #[test]
    fn mirror_pull_once_initializes_bare_repo_when_missing() {
        let tmp = TempDir::new().unwrap();
        let source_dir = tmp.path().join("source");
        std::fs::create_dir_all(&source_dir).unwrap();
        let _head = init_source_repo(&source_dir, "README.md", "hello\n");

        let bare_path = tmp.path().join("mirror/test.git");
        let manifest = build_manifest(&file_url(&source_dir), &bare_path);

        let report = mirror_pull_once(&manifest, None).expect("pull succeeds");
        assert!(report.initialized);
        assert!(
            is_bare_repo(&bare_path),
            "bare repo should exist at {}",
            bare_path.display()
        );
    }

    // 2
    #[test]
    fn mirror_pull_once_fetches_commits_from_source() {
        let tmp = TempDir::new().unwrap();
        let source_dir = tmp.path().join("source");
        std::fs::create_dir_all(&source_dir).unwrap();
        let source_head = init_source_repo(&source_dir, "README.md", "hello\n");

        let bare_path = tmp.path().join("mirror/test.git");
        let manifest = build_manifest(&file_url(&source_dir), &bare_path);

        let report = mirror_pull_once(&manifest, None).expect("pull succeeds");
        assert_eq!(report.head_after.as_deref(), Some(source_head.as_str()));

        let out = Command::new("git")
            .arg("-C")
            .arg(&bare_path)
            .arg("rev-parse")
            .arg("refs/heads/main")
            .output()
            .expect("rev-parse");
        assert!(out.status.success());
        let bare_head = String::from_utf8_lossy(&out.stdout).trim().to_string();
        assert_eq!(bare_head, source_head);
    }

    // 3
    #[test]
    fn mirror_pull_once_is_idempotent_when_source_unchanged() {
        let tmp = TempDir::new().unwrap();
        let source_dir = tmp.path().join("source");
        std::fs::create_dir_all(&source_dir).unwrap();
        let source_head = init_source_repo(&source_dir, "README.md", "hello\n");

        let bare_path = tmp.path().join("mirror/test.git");
        let manifest = build_manifest(&file_url(&source_dir), &bare_path);

        let first = mirror_pull_once(&manifest, None).expect("first pull");
        assert!(first.initialized);
        assert_eq!(first.head_after.as_deref(), Some(source_head.as_str()));

        let second = mirror_pull_once(&manifest, None).expect("second pull");
        assert!(!second.initialized);
        assert!(!second.refs_advanced);
        assert_eq!(second.head_after, first.head_after);
    }

    // 4
    #[test]
    fn mirror_pull_once_detects_new_commits_from_source() {
        let tmp = TempDir::new().unwrap();
        let source_dir = tmp.path().join("source");
        std::fs::create_dir_all(&source_dir).unwrap();
        let first_head = init_source_repo(&source_dir, "README.md", "hello\n");

        let bare_path = tmp.path().join("mirror/test.git");
        let manifest = build_manifest(&file_url(&source_dir), &bare_path);

        let first = mirror_pull_once(&manifest, None).expect("first pull");
        assert_eq!(first.head_after.as_deref(), Some(first_head.as_str()));

        let second_head = add_commit(&source_dir, "README.md", "hello, world\n");
        assert_ne!(first_head, second_head);

        let second = mirror_pull_once(&manifest, None).expect("second pull");
        assert!(!second.initialized);
        assert!(second.refs_advanced);
        assert_eq!(second.head_after.as_deref(), Some(second_head.as_str()));
    }

    // 5
    #[test]
    fn mirror_pull_once_fails_with_useful_error_on_invalid_url() {
        let tmp = TempDir::new().unwrap();
        let bare_path = tmp.path().join("mirror/test.git");
        let manifest = build_manifest(
            "file:///var/empty/symbiotic-nonexistent-definitely.git",
            &bare_path,
        );

        let err = mirror_pull_once(&manifest, None).expect_err("fetch should fail");
        match err {
            RepoMirrorError::GitCommand { op, stderr, .. } => {
                assert_eq!(op, "fetch");
                assert!(
                    !stderr.is_empty(),
                    "expected non-empty stderr for bogus remote"
                );
            }
            other => panic!("expected GitCommand{{op:\"fetch\",..}}, got {other:?}"),
        }
    }

    /// List `*.md` files under the repo-events directory for a given project.
    fn list_event_files(archive_root: &Path, project_slug: &str) -> Vec<PathBuf> {
        let events_dir = archive_root
            .join("operations")
            .join("projects")
            .join(project_slug)
            .join("repos")
            .join("events");
        let mut out = Vec::new();
        if let Ok(iter) = std::fs::read_dir(&events_dir) {
            for entry in iter.flatten() {
                let path = entry.path();
                if path.is_file()
                    && path
                        .extension()
                        .and_then(|ext| ext.to_str())
                        .map(|ext| ext == "md")
                        .unwrap_or(false)
                {
                    out.push(path);
                }
            }
        }
        out.sort();
        out
    }

    // 7
    #[test]
    fn mirror_pull_emits_event_on_success_when_context_provided() {
        let tmp = TempDir::new().unwrap();
        let source_dir = tmp.path().join("source");
        std::fs::create_dir_all(&source_dir).unwrap();
        let _head = init_source_repo(&source_dir, "README.md", "hello\n");

        let bare_path = tmp.path().join("mirror/test.git");
        let manifest = build_manifest(&file_url(&source_dir), &bare_path);

        let archive_root = tmp.path().join("archive");
        std::fs::create_dir_all(&archive_root).unwrap();
        let ctx = RepoMirrorArchiveContext {
            archive_root: &archive_root,
            project_id: "project:test",
            observed_at: 1_700_000_000,
        };

        let report = mirror_pull_once(&manifest, Some(&ctx)).expect("pull succeeds");
        assert!(report.initialized);

        let files = list_event_files(&archive_root, "test");
        assert_eq!(files.len(), 1, "one event file written: {files:?}");
        let contents = std::fs::read_to_string(&files[0]).unwrap();
        assert!(
            contents.contains("event_type: \"repo_mirror_pull_completed\""),
            "event type present: {contents}"
        );
        assert!(
            contents.contains("repo_id: \"repo:test\""),
            "repo_id present: {contents}"
        );
    }

    // 8
    #[test]
    fn mirror_pull_no_event_when_context_none() {
        let tmp = TempDir::new().unwrap();
        let source_dir = tmp.path().join("source");
        std::fs::create_dir_all(&source_dir).unwrap();
        let _head = init_source_repo(&source_dir, "README.md", "hello\n");

        let bare_path = tmp.path().join("mirror/test.git");
        let manifest = build_manifest(&file_url(&source_dir), &bare_path);

        let archive_root = tmp.path().join("archive");
        std::fs::create_dir_all(&archive_root).unwrap();

        let _report = mirror_pull_once(&manifest, None).expect("pull succeeds");

        let files = list_event_files(&archive_root, "test");
        assert!(
            files.is_empty(),
            "no events should be written without a context: {files:?}"
        );
    }

    // 9
    #[test]
    fn mirror_pull_event_survives_archive_write_failure() {
        // Force `append_repo_event_archive` to fail by pointing `archive_root`
        // at a path that is a regular file rather than a directory: the helper
        // calls `fs::create_dir_all(archive_root/operations/...)` which fails
        // when any ancestor is a file. The invariant under test: event-write
        // failure must NOT fail the pull.
        let tmp = TempDir::new().unwrap();
        let source_dir = tmp.path().join("source");
        std::fs::create_dir_all(&source_dir).unwrap();
        let _head = init_source_repo(&source_dir, "README.md", "hello\n");

        let bare_path = tmp.path().join("mirror/test.git");
        let manifest = build_manifest(&file_url(&source_dir), &bare_path);

        let blocking_file = tmp.path().join("blocking-file");
        std::fs::write(&blocking_file, b"i am a file, not a dir").unwrap();
        let ctx = RepoMirrorArchiveContext {
            archive_root: &blocking_file,
            project_id: "project:test",
            observed_at: 1_700_000_000,
        };

        let report = mirror_pull_once(&manifest, Some(&ctx))
            .expect("pull should succeed even when event-write fails");
        assert!(report.initialized);
    }

    // ── Push tests (§07b.ii-a) ────────────────────────────────────────

    use credential_gateway::{CredentialRecord, GoalScopedVault};

    /// Seed a `GoalScopedVault` at `dir` with a dummy credential under the
    /// global namespace for `credential_id`. `file://` push does not consult
    /// the secret, but `with_git_push_session` requires the entry exist.
    fn seed_vault(dir: &Path, credential_id: &str) -> GoalScopedVault {
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
        vault
    }

    /// Create two bare repos (`source.git`, `internal.git`) in `ctx` plus a
    /// seed commit on `main` in `internal.git` (so there is something to
    /// push). Returns (source_bare, internal_bare, branch, credential_id).
    fn setup_push_fixture(ctx: &TempDir) -> (PathBuf, PathBuf, String, String) {
        let source_bare = ctx.path().join("source.git");
        let mut init_src = Command::new("git");
        init_src
            .arg("init")
            .arg("--bare")
            .arg(&source_bare)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null");
        must_succeed(init_src, "init source.git");

        let internal_bare = ctx.path().join("internal.git");
        let mut init_int = Command::new("git");
        init_int
            .arg("init")
            .arg("--bare")
            .arg(&internal_bare)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null");
        must_succeed(init_int, "init internal.git");

        // Clone internal.git into a throwaway worktree, commit a file, and
        // push back to internal.git so it has a `main` ref.
        let worktree = ctx.path().join("seed");
        let mut clone = Command::new("git");
        clone
            .arg("clone")
            .arg(&internal_bare)
            .arg(&worktree)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null");
        must_succeed(clone, "clone internal.git to seed");

        std::fs::write(worktree.join("README.md"), "hello push\n").unwrap();

        let mut checkout = git_cmd(&worktree);
        checkout.arg("checkout").arg("-B").arg("main");
        must_succeed(checkout, "checkout -B main");

        let mut add = git_cmd(&worktree);
        add.arg("add").arg("README.md");
        must_succeed(add, "git add");

        let mut commit = git_cmd(&worktree);
        commit.arg("commit").arg("-m").arg("seed");
        must_succeed(commit, "git commit seed");

        let mut push = git_cmd(&worktree);
        push.arg("push").arg("origin").arg("main");
        must_succeed(push, "git push origin main");

        (
            source_bare,
            internal_bare,
            "main".to_string(),
            "cred:push-test".to_string(),
        )
    }

    /// Build a push-capable manifest.
    fn build_push_manifest(
        source_url: &str,
        internal_bare_path: &Path,
        credential_id: &str,
        push_external: bool,
    ) -> RepoManifest {
        RepoManifest {
            id: "repo:test-push".to_string(),
            project_id: "project:test".to_string(),
            slug: "test".to_string(),
            title: "test".to_string(),
            state: RepoState::Active,
            repo_role: RepoRole::Source,
            source: RepoSource {
                url: source_url.to_string(),
                provider: RepoProvider::Local,
                default_branch: "main".to_string(),
                protected_branches: vec!["main".to_string()],
                pinned_head: None,
            },
            credential: RepoCredentialBinding {
                id: credential_id.to_string(),
                scope: CredentialScope::Push,
                trust_floor: AgentTrustLevel::ReadOnly,
            },
            mirror: RepoMirrorPolicy {
                internal_bare_path: internal_bare_path.to_path_buf(),
                direction: MirrorDirection::Bidirectional,
                sync_interval_secs: 300,
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

    // 10
    #[test]
    fn mirror_push_once_file_url_succeeds() {
        let tmp = TempDir::new().unwrap();
        let (source_bare, internal_bare, branch, credential_id) = setup_push_fixture(&tmp);

        let vault_dir = tmp.path().join("vault");
        let vault = seed_vault(&vault_dir, &credential_id);

        let source_url = format!("file://{}", source_bare.display());
        let manifest = build_push_manifest(&source_url, &internal_bare, &credential_id, true);

        let report = mirror_push_once(&manifest, "agent-a", &vault, None, &branch, None)
            .expect("push succeeds");
        assert!(report.pushed);
        assert!(report.head_after.is_some(), "head_after populated");
        assert!(report.head_before.is_none(), "head_before stays None");

        // Confirm source.git now has the commit on refs/heads/main.
        let out = Command::new("git")
            .arg("-C")
            .arg(&source_bare)
            .arg("rev-parse")
            .arg("refs/heads/main")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .expect("spawn rev-parse source");
        assert!(
            out.status.success(),
            "source.git should have refs/heads/main after push; stderr={}",
            String::from_utf8_lossy(&out.stderr)
        );
        let source_head = String::from_utf8_lossy(&out.stdout).trim().to_string();
        assert_eq!(
            Some(source_head.as_str()),
            report.head_after.as_deref(),
            "source HEAD matches the reported head_after"
        );
    }

    // 11
    #[test]
    fn mirror_push_once_rejects_when_push_external_disabled() {
        let tmp = TempDir::new().unwrap();
        let (source_bare, internal_bare, branch, credential_id) = setup_push_fixture(&tmp);

        let vault_dir = tmp.path().join("vault");
        let vault = seed_vault(&vault_dir, &credential_id);

        let source_url = format!("file://{}", source_bare.display());
        // push_external: false → gate rejection.
        let manifest = build_push_manifest(&source_url, &internal_bare, &credential_id, false);

        let err = mirror_push_once(&manifest, "agent-a", &vault, None, &branch, None)
            .expect_err("push should be gated");
        match err {
            RepoMirrorError::PushExternalDisabled { id } => {
                assert_eq!(id, manifest.id);
            }
            other => panic!("expected PushExternalDisabled, got {other:?}"),
        }
    }

    // 12
    #[test]
    fn mirror_push_once_emits_push_completed_event_with_agent_id() {
        let tmp = TempDir::new().unwrap();
        let (source_bare, internal_bare, branch, credential_id) = setup_push_fixture(&tmp);

        let vault_dir = tmp.path().join("vault");
        let vault = seed_vault(&vault_dir, &credential_id);

        let source_url = format!("file://{}", source_bare.display());
        let manifest = build_push_manifest(&source_url, &internal_bare, &credential_id, true);

        let archive_root = tmp.path().join("archive");
        std::fs::create_dir_all(&archive_root).unwrap();
        let ctx = RepoMirrorArchiveContext {
            archive_root: &archive_root,
            project_id: "project:test",
            observed_at: 1_700_000_000,
        };

        let report = mirror_push_once(&manifest, "agent-a", &vault, Some(&ctx), &branch, None)
            .expect("push succeeds");
        assert!(report.pushed);

        let files = list_event_files(&archive_root, "test");
        assert_eq!(files.len(), 1, "one event file written: {files:?}");
        let contents = std::fs::read_to_string(&files[0]).unwrap();
        assert!(
            contents.contains("event_type: \"repo_mirror_push_completed\""),
            "event type present: {contents}"
        );
        assert!(
            contents.contains("agent_id: \"agent-a\""),
            "agent_id present in frontmatter: {contents}"
        );
    }

    // 13
    #[test]
    fn mirror_push_once_credential_missing_returns_typed_error() {
        let tmp = TempDir::new().unwrap();
        let (source_bare, internal_bare, branch, _seeded_credential_id) = setup_push_fixture(&tmp);

        // Seed vault under a DIFFERENT credential id than the manifest references.
        let vault_dir = tmp.path().join("vault");
        let vault = seed_vault(&vault_dir, "cred:some-other-id");

        let source_url = format!("file://{}", source_bare.display());
        // Manifest points at a credential id that was never seeded.
        let manifest = build_push_manifest(&source_url, &internal_bare, "cred:nonexistent", true);

        let err = mirror_push_once(&manifest, "agent-a", &vault, None, &branch, None)
            .expect_err("push should fail on missing credential");
        match err {
            RepoMirrorError::PushCredentialMissing(id) => {
                assert_eq!(id, "cred:nonexistent");
            }
            other => panic!("expected PushCredentialMissing, got {other:?}"),
        }
    }

    // 14
    #[test]
    fn mirror_push_once_no_event_when_context_none() {
        let tmp = TempDir::new().unwrap();
        let (source_bare, internal_bare, branch, credential_id) = setup_push_fixture(&tmp);

        let vault_dir = tmp.path().join("vault");
        let vault = seed_vault(&vault_dir, &credential_id);

        let source_url = format!("file://{}", source_bare.display());
        let manifest = build_push_manifest(&source_url, &internal_bare, &credential_id, true);

        let archive_root = tmp.path().join("archive");
        std::fs::create_dir_all(&archive_root).unwrap();

        let report = mirror_push_once(&manifest, "agent-a", &vault, None, &branch, None)
            .expect("push succeeds");
        assert!(report.pushed);

        // The repo-events dir for this project should not exist.
        let events_dir = archive_root
            .join("operations")
            .join("projects")
            .join("test")
            .join("repos")
            .join("events");
        assert!(
            !events_dir.exists(),
            "no events dir should be created without a context: {}",
            events_dir.display()
        );
    }

    // ── Approval-gated wrapper tests (§07b.ii-b) ──────────────────────

    use crate::approval_gate::{ApprovalGate, ApprovalState};
    use crate::matrix_poster::MatrixPoster;
    use crate::push::{PushNotification, PushProvider};
    use std::sync::Arc as StdArc;
    use std::sync::Mutex as StdMutex;

    struct MockMatrixPoster {
        posted: std::sync::Mutex<Vec<(String, String)>>,
    }

    impl MockMatrixPoster {
        fn new() -> Self {
            Self {
                posted: std::sync::Mutex::new(Vec::new()),
            }
        }
        fn count(&self) -> usize {
            self.posted.lock().unwrap().len()
        }
    }

    #[async_trait::async_trait]
    impl MatrixPoster for MockMatrixPoster {
        async fn post_text(&self, room_id: &str, body: &str, _now: u64) -> anyhow::Result<()> {
            self.posted
                .lock()
                .unwrap()
                .push((room_id.to_string(), body.to_string()));
            Ok(())
        }
    }

    struct MockPushProvider {
        sent: std::sync::Mutex<Vec<PushNotification>>,
    }

    impl MockPushProvider {
        fn new() -> Self {
            Self {
                sent: std::sync::Mutex::new(Vec::new()),
            }
        }
        fn count(&self) -> usize {
            self.sent.lock().unwrap().len()
        }
    }

    impl PushProvider for MockPushProvider {
        fn send(&self, notif: &PushNotification) -> anyhow::Result<()> {
            self.sent.lock().unwrap().push(notif.clone());
            Ok(())
        }
    }

    /// Variant of `build_push_manifest` that lets the caller specify
    /// `requires_operator_approval_for`.
    fn build_push_manifest_with_approval(
        source_url: &str,
        internal_bare_path: &Path,
        credential_id: &str,
        push_external: bool,
        requires_approval_for: Vec<String>,
    ) -> RepoManifest {
        let mut m =
            build_push_manifest(source_url, internal_bare_path, credential_id, push_external);
        m.agent_scopes.requires_operator_approval_for = requires_approval_for;
        m
    }

    // 15
    #[tokio::test]
    async fn mirror_push_with_approval_skips_gate_when_not_required() {
        let tmp = TempDir::new().unwrap();
        let (source_bare, internal_bare, branch, credential_id) = setup_push_fixture(&tmp);

        let vault_dir = tmp.path().join("vault");
        let vault = seed_vault(&vault_dir, &credential_id);

        let source_url = format!("file://{}", source_bare.display());
        let manifest = build_push_manifest_with_approval(
            &source_url,
            &internal_bare,
            &credential_id,
            true,
            vec![], // empty → no approval needed
        );

        let gate = StdArc::new(StdMutex::new(ApprovalGate::new()));
        let poster = MockMatrixPoster::new();
        let provider = MockPushProvider::new();

        let report = mirror_push_with_approval(
            &manifest,
            "agent-a",
            &vault,
            &gate,
            &poster,
            &provider,
            "!room:test",
            None,
            &branch,
            None,
            None,
            60,
            50,
        )
        .await
        .expect("wrapper skips gate and push succeeds");

        assert!(report.pushed);
        assert_eq!(
            poster.count(),
            0,
            "matrix poster must not be invoked when approval not required"
        );
        assert_eq!(
            provider.count(),
            0,
            "push provider must not be invoked when approval not required"
        );
    }

    // 16
    #[tokio::test]
    async fn mirror_push_with_approval_blocks_then_proceeds_on_approve() {
        let tmp = TempDir::new().unwrap();
        let (source_bare, internal_bare, branch, credential_id) = setup_push_fixture(&tmp);

        let vault_dir = tmp.path().join("vault");
        let vault = seed_vault(&vault_dir, &credential_id);

        let source_url = format!("file://{}", source_bare.display());
        let manifest = build_push_manifest_with_approval(
            &source_url,
            &internal_bare,
            &credential_id,
            true,
            vec!["push_external".to_string()],
        );

        let gate = StdArc::new(StdMutex::new(ApprovalGate::new()));
        let poster = StdArc::new(MockMatrixPoster::new());
        let provider = StdArc::new(MockPushProvider::new());

        let gate_clone = gate.clone();
        let poster_clone = poster.clone();
        let provider_clone = provider.clone();
        let branch_clone = branch.clone();
        let manifest_clone = manifest.clone();

        let handle = tokio::spawn(async move {
            mirror_push_with_approval(
                &manifest_clone,
                "agent-a",
                &vault,
                &gate_clone,
                poster_clone.as_ref(),
                provider_clone.as_ref(),
                "!room:test",
                None,
                &branch_clone,
                Some("goal-1"),
                None,
                60,
                50,
            )
            .await
        });

        // Wait for the wrapper to open the ticket.
        let ticket_id = loop {
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
            let guard = gate.lock().unwrap();
            if let Some(t) = guard.list_pending().first() {
                break t.ticket_id.clone();
            }
        };

        // Approve it.
        {
            let mut guard = gate.lock().unwrap();
            guard
                .approve(&ticket_id, "operator", symbiotic_core::now_unix())
                .expect("approve ok");
        }

        let report = handle.await.expect("task join").expect("wrapper ok");
        assert!(report.pushed);
        assert_eq!(poster.count(), 1, "matrix poster invoked exactly once");
        assert_eq!(provider.count(), 1, "push provider invoked exactly once");
    }

    // 17
    #[tokio::test]
    async fn mirror_push_with_approval_returns_denied_error() {
        let tmp = TempDir::new().unwrap();
        let (source_bare, internal_bare, branch, credential_id) = setup_push_fixture(&tmp);

        let vault_dir = tmp.path().join("vault");
        let vault = seed_vault(&vault_dir, &credential_id);

        let source_url = format!("file://{}", source_bare.display());
        let manifest = build_push_manifest_with_approval(
            &source_url,
            &internal_bare,
            &credential_id,
            true,
            vec!["push_external".to_string()],
        );

        let gate = StdArc::new(StdMutex::new(ApprovalGate::new()));
        let poster = StdArc::new(MockMatrixPoster::new());
        let provider = StdArc::new(MockPushProvider::new());

        let gate_clone = gate.clone();
        let poster_clone = poster.clone();
        let provider_clone = provider.clone();
        let branch_clone = branch.clone();
        let manifest_clone = manifest.clone();

        let handle = tokio::spawn(async move {
            mirror_push_with_approval(
                &manifest_clone,
                "agent-a",
                &vault,
                &gate_clone,
                poster_clone.as_ref(),
                provider_clone.as_ref(),
                "!room:test",
                None,
                &branch_clone,
                None,
                None,
                60,
                50,
            )
            .await
        });

        let ticket_id = loop {
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
            let guard = gate.lock().unwrap();
            if let Some(t) = guard.list_pending().first() {
                break t.ticket_id.clone();
            }
        };

        {
            let mut guard = gate.lock().unwrap();
            guard
                .deny(
                    &ticket_id,
                    "operator",
                    symbiotic_core::now_unix(),
                    Some("nope".to_string()),
                )
                .expect("deny ok");
        }

        let err = handle
            .await
            .expect("task join")
            .expect_err("wrapper should err on deny");
        match err {
            RepoMirrorError::ApprovalDenied { reason } => {
                assert_eq!(reason.as_deref(), Some("nope"));
            }
            other => panic!("expected ApprovalDenied, got {other:?}"),
        }
    }

    // 18
    #[tokio::test]
    async fn mirror_push_with_approval_expires_after_ttl() {
        let tmp = TempDir::new().unwrap();
        let (source_bare, internal_bare, branch, credential_id) = setup_push_fixture(&tmp);

        let vault_dir = tmp.path().join("vault");
        let vault = seed_vault(&vault_dir, &credential_id);

        let source_url = format!("file://{}", source_bare.display());
        let manifest = build_push_manifest_with_approval(
            &source_url,
            &internal_bare,
            &credential_id,
            true,
            vec!["push_external".to_string()],
        );

        let gate = StdArc::new(StdMutex::new(ApprovalGate::new()));
        let poster = MockMatrixPoster::new();
        let provider = MockPushProvider::new();

        let err = mirror_push_with_approval(
            &manifest,
            "agent-a",
            &vault,
            &gate,
            &poster,
            &provider,
            "!room:test",
            None,
            &branch,
            None,
            None,
            1, // ttl_secs = 1
            50,
        )
        .await
        .expect_err("wrapper should expire");

        match err {
            RepoMirrorError::ApprovalExpired { ticket_id } => {
                assert!(!ticket_id.is_empty());
            }
            other => panic!("expected ApprovalExpired, got {other:?}"),
        }
    }

    // 19
    #[tokio::test]
    async fn mirror_push_with_approval_emits_request_and_outcome_events() {
        let tmp = TempDir::new().unwrap();
        let (source_bare, internal_bare, branch, credential_id) = setup_push_fixture(&tmp);

        let vault_dir = tmp.path().join("vault");
        let vault = seed_vault(&vault_dir, &credential_id);

        let source_url = format!("file://{}", source_bare.display());
        let manifest = build_push_manifest_with_approval(
            &source_url,
            &internal_bare,
            &credential_id,
            true,
            vec!["push_external".to_string()],
        );

        let archive_root = tmp.path().join("archive");
        std::fs::create_dir_all(&archive_root).unwrap();

        let gate = StdArc::new(StdMutex::new(ApprovalGate::new()));
        let poster = StdArc::new(MockMatrixPoster::new());
        let provider = StdArc::new(MockPushProvider::new());

        let gate_clone = gate.clone();
        let poster_clone = poster.clone();
        let provider_clone = provider.clone();
        let branch_clone = branch.clone();
        let manifest_clone = manifest.clone();
        let archive_root_clone = archive_root.clone();

        let handle = tokio::spawn(async move {
            let ctx = RepoMirrorArchiveContext {
                archive_root: &archive_root_clone,
                project_id: "project:test",
                observed_at: 1_700_000_000,
            };
            mirror_push_with_approval(
                &manifest_clone,
                "agent-a",
                &vault,
                &gate_clone,
                poster_clone.as_ref(),
                provider_clone.as_ref(),
                "!room:test",
                Some(&ctx),
                &branch_clone,
                Some("goal-1"),
                None,
                60,
                50,
            )
            .await
        });

        let ticket_id = loop {
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
            let guard = gate.lock().unwrap();
            if let Some(t) = guard.list_pending().first() {
                break t.ticket_id.clone();
            }
        };

        {
            let mut guard = gate.lock().unwrap();
            guard
                .approve(&ticket_id, "operator", symbiotic_core::now_unix())
                .expect("approve ok");
        }

        let report = handle.await.expect("task join").expect("wrapper ok");
        assert!(report.pushed);

        // Expect three event files: requested + approved + push_completed.
        let files = list_event_files(&archive_root, "test");
        let types: Vec<String> = files
            .iter()
            .map(|p| std::fs::read_to_string(p).unwrap())
            .collect();

        let has = |needle: &str| types.iter().any(|c| c.contains(needle));
        assert!(
            has("event_type: \"repo_external_push_approval_requested\""),
            "missing requested event: {types:?}"
        );
        assert!(
            has("event_type: \"repo_external_push_approval_approved\""),
            "missing approved event: {types:?}"
        );
        assert!(
            has("event_type: \"repo_mirror_push_completed\""),
            "missing push_completed event: {types:?}"
        );

        // Silence unused warnings in this test.
        let _ = ApprovalState::Pending;
    }
}
