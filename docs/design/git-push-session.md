# Git Push Session — External Git-Push Credential Primitive

**Status**: Proposed Specification
**Related Tasks**: T126 §07 (first consumer), T128 (future consumer for Source Archeology patchsets)
**Related Docs**: `docs/design/repo-manifest.md`, `docs/design/credential-sandbox.md`, `docs/design/internal-git-swarm.md`

## Problem

`docs/design/repo-manifest.md` §Integration specifies that the daemon performs a `git push {internal_bare_path} {source.url}` from the durable bare mirror to the external remote (GitHub, GitLab, etc.). The push needs an authenticated git invocation — SSH key or HTTPS credential — but **the raw credential must never leak to any agent process**.

Existing primitives are close but don't fit:

- `credential-gateway::SessionHandle` is a session-token abstraction designed for login flows; it references a session, doesn't materialize credentials for subprocess use.
- `credential-gateway::OneShotCredentialLease` (from `docs/design/credential-sandbox.md` §Auth Sandbox Worker) is scoped to the auth sandbox worker subprocess, not a daemon-side git shell-out.
- The internal `X-Symbiotic-Push-Session` from `docs/design/internal-git-swarm.md` §86-90 is for agent → internal bare HTTP pushes; it has nothing to do with external credentials.

We need a **scope-minimal per-API connector** for git-push that follows the architecture resolved in `tasks/NEXT.md`'s T128 D1 decision ("each sandbox→host capability gets its own scope-minimal primitive"). T126 operator directive D1a (2026-04-17) approved building `GitPushSession` as a sibling connector to the existing push-session primitive.

## Non-Goals

- Not a replacement for `SessionHandle` / `OneShotCredentialLease` / internal push sessions — sibling, not superseding.
- Not a login primitive. Credentials are already stored in `GoalScopedVault` by the time a push happens; `GitPushSession` consumes stored credentials, it does not capture new ones.
- Not a push-authorization gate. Authorization (role gate, push_external flag, approval gate) lives above in `repo_capabilities` + T126 §08. `GitPushSession` presumes the caller has already gated the operation.
- Not agent-visible. The daemon is the sole caller. Agents never hold a `GitPushSession` handle or credential.

## Core Contract

`GitPushSession` is a **scope-guarded credential materialization primitive** for git subprocess invocation. It has two invariants:

1. **The credential never outlives the closure scope.** Callers invoke with a closure receiving a `GitPushEnv`; on closure return (normal or panic), all materialized state is wiped — env vars cleared, tempfiles unlinked, vault lease released.
2. **The credential never enters the daemon's general process env.** Env vars are set on the child `Command` only, via `.env(K, V)` — they don't leak to the daemon's `std::env`.

### Primary API

```rust
// In a new crate: submodules/runtime/services/credential-gateway/src/git_push_session.rs
// Re-exported from credential-gateway root.

/// Credential kind — determines how the credential materializes for git.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitCredentialKind {
    /// SSH private key. Materialized as a tempfile + GIT_SSH_COMMAND env var.
    SshPrivateKey,
    /// HTTPS token (PAT, OAuth2 bearer, etc.). Materialized via GIT_ASKPASS + a
    /// short-lived askpass script that echoes the token.
    HttpsToken,
}

/// The materialized env for a single git invocation. Implements no Debug trait
/// that reveals the credential — all fields are explicitly redacted.
pub struct GitPushEnv {
    /// Env vars to pass to `Command.envs(...)`. NEVER logged.
    pub(crate) env_vars: Vec<(String, String)>,
    /// Tempfile paths owned by the session (private key file, askpass script).
    /// Dropped when the session ends; files are explicitly removed on drop.
    _owned_tempfiles: Vec<TempFile>,
}

impl GitPushEnv {
    /// Apply this env to a git Command invocation. Callers invoke .envs() internally.
    pub fn apply(&self, cmd: &mut std::process::Command);
}

/// Fully custom Debug implementation that prints NO credential data.
impl std::fmt::Debug for GitPushEnv { ... }

/// Scope-guarded materialization. All cleanup happens when `f` returns.
///
/// `credential_id` is the opaque handle stored on `RepoCredentialBinding.id` —
/// resolved here via `vault.get_credential(credential_id)`.
///
/// `kind` is derived by the caller from `RepoSource.url`:
///   - `git@github.com:...` / `ssh://...` → `SshPrivateKey`
///   - `https://...` → `HttpsToken`
///
/// `f` receives a borrowed `GitPushEnv` and runs the git subprocess. The
/// returned R is propagated. Any panic in `f` still triggers full cleanup
/// via the `_owned_tempfiles` drop + the vault lease's own drop.
pub fn with_git_push_session<R, F>(
    vault: &GoalScopedVault,
    credential_id: &str,
    kind: GitCredentialKind,
    goal_scope: Option<&str>,
    f: F,
) -> Result<R, GitPushSessionError>
where
    F: FnOnce(&GitPushEnv) -> Result<R, GitPushSessionError>;

#[derive(Debug, Error)]
pub enum GitPushSessionError {
    #[error("credential not found: {0}")]
    CredentialNotFound(String),
    #[error("credential scope denied: {0}")]
    ScopeDenied(String),
    #[error("io error materializing credential: {0}")]
    Io(#[from] std::io::Error),
    #[error("subprocess failed: {0}")]
    Subprocess(String),
}
```

### Caller Pattern (from T126 §07)

```rust
// In repo_mirror.rs::mirror_push_once:
credential_gateway::with_git_push_session(
    vault,
    &manifest.credential.id,
    kind_from_url(&manifest.source.url),
    goal_scope.as_deref(),
    |env| {
        let mut cmd = std::process::Command::new("git");
        cmd.arg("-C").arg(&manifest.mirror.internal_bare_path)
           .arg("push").arg("origin").arg(refspec);
        env.apply(&mut cmd);
        // author identity threaded in by §07 caller, see below
        cmd.env("GIT_AUTHOR_NAME", format!("Symbiotic Agent {agent_id}"))
           .env("GIT_AUTHOR_EMAIL", format!("agent-{agent_id}@symbiotic.sh"))
           .env("GIT_COMMITTER_NAME", format!("Symbiotic Agent {agent_id}"))
           .env("GIT_COMMITTER_EMAIL", format!("agent-{agent_id}@symbiotic.sh"));
        let output = cmd.output()?;
        if !output.status.success() {
            return Err(GitPushSessionError::Subprocess(
                String::from_utf8_lossy(&output.stderr).into_owned()
            ));
        }
        Ok(output)
    }
)?;
```

## Materialization Details

### SSH Private Key

1. `vault.get_credential(credential_id)` → `CredentialRecord { service, username, secret, .. }`. Secret is the ASCII-armored private key contents.
2. Create tempfile in `std::env::temp_dir()` with mode `0600`. Write `secret` to it.
3. Build `ssh_command_value = format!("ssh -i {tempfile_path} -o StrictHostKeyChecking=accept-new -o UserKnownHostsFile=/dev/null -o IdentitiesOnly=yes")`.
4. `env_vars = vec![("GIT_SSH_COMMAND", ssh_command_value)]`.
5. `_owned_tempfiles = vec![tempfile]` — drop unlinks.

Rationale for `StrictHostKeyChecking=accept-new`: first-time host verification is auto-accepted; subsequent changes would be rejected. `UserKnownHostsFile=/dev/null` prevents the session from polluting the user's known_hosts. `IdentitiesOnly=yes` prevents ssh-agent fallback that might use a different key.

### HTTPS Token

1. Resolve the credential record as above. `secret` is the PAT / OAuth2 bearer / etc.
2. Create an askpass script tempfile:
   ```bash
   #!/bin/sh
   case "$1" in
     Username*) echo "{username}" ;;
     Password*) echo "{secret}" ;;
   esac
   ```
   Write with mode `0700`.
3. `env_vars = vec![("GIT_ASKPASS", script_path), ("GIT_TERMINAL_PROMPT", "0")]`.
4. `_owned_tempfiles = vec![script_tempfile]` — drop unlinks.

`GIT_TERMINAL_PROMPT=0` prevents git from falling back to interactive prompts if the askpass exits nonzero.

### Shared invariants

- Tempfile perms set **before** writing the secret (race-free). Use `std::fs::OpenOptions::new().create_new(true).mode(0o600).open(path)` on Unix.
- Tempfile paths use `format!("symbiotic-push-{pid}-{uuid}")` to avoid collisions and to make post-mortem cleanup trivial for operators.
- `TempFile` type wraps the path and implements `Drop` that unlinks — even if the process crashes mid-push, tempfiles are cleaned up on next daemon start via a tempdir prefix scan (defensive).

## Vault Integration

The vault's existing API (`GoalScopedVault::get_credential(id)`) returns a `Result<CredentialRecord>`. `with_git_push_session` calls this once up front, materializes, and then the `CredentialRecord` is dropped. The secret string lives in memory briefly during step 2/tempfile write — this is unavoidable for any implementation that uses git subprocess, but the window is sub-millisecond and the daemon process is single-tenant.

The `goal_scope` parameter flows through to `GoalScopedVault::get_credential_scoped` when present, enforcing the existing goal-boundary isolation (see `credential-gateway/src/lib.rs` §291-396). For push operations from the durable mirror (not inside a goal), pass `None`.

## Security Notes

- **Secret never enters `std::env`**: all env vars are set on the child `Command` directly. The daemon's own process env is unchanged.
- **Secret never reaches any agent process**: `with_git_push_session` is a daemon-side helper; agents call into the daemon via RPC to trigger pushes but never receive the materialized `GitPushEnv`.
- **Secret never leaks via logging**: `GitPushEnv::Debug` prints nothing revealing; `CredentialRecord::Debug` already redacts (see `credential-gateway/src/lib.rs:78-89`). The git subprocess's stderr is captured and logged — but git itself is careful not to echo secrets back in errors. If operators observe secrets in logs from a git subprocess, that's a bug to fix in git invocation (not in this primitive).
- **Tempfiles are short-lived and cleaned on drop**: `TempFile.drop()` unlinks. Panics trigger drop just like normal returns.
- **No process env propagation**: we do NOT use `.env_clear()` because agents' git config (user.name / user.email defaults) is set by the caller explicitly; bare-repo pushes don't need the user's shell env.
- **First-use host verification**: SSH path uses `accept-new`, not `no`. A man-in-the-middle on first push is possible; subsequent pushes are protected. For stronger guarantees, future work could pre-populate a known_hosts file from `source.provider` (GitHub / GitLab publish their host keys).

## Crate Location

Recommended: live inside `submodules/runtime/services/credential-gateway/` as a new module `git_push_session.rs`. Re-exported from the crate root alongside the existing `SessionHandle` / `CredentialRecord` / `GoalScopedVault`. Rationale: credential primitives belong together, and this sibling fits the existing crate's "host-side credential plane" purpose.

Alternative: new crate `symbiotic-git-push-session`. Rejected because it duplicates infrastructure (vault access, error types, Cargo dep churn) without a meaningful boundary.

## Failure Modes

- **Vault miss** (`credential_id` not found) → `CredentialNotFound`. T126 §07 surfaces this as `RepoMirrorError::PushCredentialMissing`.
- **Scope denied** (credential exists but `goal_scope` doesn't match) → `ScopeDenied`. Same surfacing pattern.
- **Tempfile creation fails** (disk full, permissions) → `Io`. Caller retries on transient, fails hard on persistent.
- **Subprocess nonzero exit** → `Subprocess(stderr)`. Caller maps to `RepoMirrorError::GitCommand`.
- **Panic in closure** → cleanup still runs (drop guarantee). Panic propagates.

## Testing

Unit tests in `credential-gateway/src/git_push_session.rs`:

1. `ssh_materializes_key_with_correct_perms` — tempfile created at mode 0600, contents match the vault secret.
2. `ssh_env_vars_set_correctly` — `GIT_SSH_COMMAND` present with expected flags.
3. `https_materializes_askpass_script` — script is mode 0700, outputs `username`/`password` on respective args.
4. `https_env_vars_set_correctly` — `GIT_ASKPASS` + `GIT_TERMINAL_PROMPT=0`.
5. `tempfile_unlinked_on_normal_return` — after `with_git_push_session` returns, tempfile path doesn't exist.
6. `tempfile_unlinked_on_panic` — `with_git_push_session` called with a closure that panics; catch the panic, assert tempfile doesn't exist.
7. `credential_not_found_returns_typed_error` — pass a bogus credential_id; expect `CredentialNotFound`.
8. `closure_result_propagates` — simple `Ok(42)` closure returns `Ok(42)`.

Integration testing (in T126 §07 chunk): file:// remote push using a SSH key fixture via `ssh-keygen -t ed25519 -f tempfile -N ""`.

## Open Questions

- **Keyed caching**: should repeated pushes to the same credential reuse a materialized env, or always re-materialize? Recommendation: **always re-materialize** for MVP (simple + secure; cache complicates the drop semantics). Revisit if push throughput becomes a bottleneck.
- **OAuth2 refresh**: if the stored credential is an OAuth2 access-token that expires, who refreshes it? Not this primitive. Caller (repo_mirror.rs) detects 401 subprocess failure, invokes a separate refresh flow, retries. Out of scope here.
- **Post-push audit hook**: should `with_git_push_session` emit a canonical event (e.g. `credential_used`) for audit? Recommendation: **no** — audit belongs in the caller (repo_mirror.rs) via the T126 §06 event helper. Keeps this primitive narrow.
