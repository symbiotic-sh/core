# Symbiotic Daemon — Stepped Context

Read this file before working on the daemon. For project-wide rules, see `../../CONTEXT.md`.

## Structure

```
services/symbiotic-daemon/
├── src/
│   ├── main.rs          # CLI entry, serve loop, signal handling, periodic snapshots
│   ├── lib.rs           # SymbioticDaemon struct, DaemonConfig, open(), job dispatch, payload codecs
│   ├── commands.rs      # ControlCommand dispatch (route_matrix_message_with_targets), push, transport
│   ├── events.rs        # DaemonEvent, DaemonStatusSnapshot, RoutedMatrixEnvelope, simple_hash
│   ├── goals.rs         # Goal start/stop/list/status, workflow execution, workflow payload codecs
│   ├── install.rs       # Install/provision/bootstrap/verify handlers and payload codecs
│   ├── agents.rs        # Agent spawning, scope execution, capability tokens, role resolution
│   ├── workers.rs       # Queue worker adapters (IngestAdapter, ReviewAdapter), payload encoding
│   ├── auth.rs          # Authentication flows
│   ├── bootstrap.rs     # First-run setup (rooms, config)
│   ├── goal_state.rs    # Goal/task state management
│   ├── routing.rs       # Event routing logic, ControlCommand enum, room classification
│   ├── secrets.rs       # Secret storage interface
│   ├── push.rs          # Push notification support (local push registry)
│   ├── push_client.rs   # Push gateway HTTP client (managed mode, calls push.symbiotic.sh)
│   ├── push_dispatcher.rs # Event-to-push routing (classifies events, dispatches to devices)
│   ├── tunnel.rs        # Relay tunnel client (outbound WSS to relay, proxies HTTP to Conduwuit)
│   └── x_intake.rs      # Intake extensions (X/Twitter)
├── Cargo.toml
└── CONTEXT.md           # This file
```

## Key Architecture

- **Not Send+Sync**: `SymbioticDaemon` holds non-Send types — no `tokio::spawn`. Use synchronous loops with `Instant::elapsed` for periodic work.
- **Tunnel client**: `tunnel.rs` — outbound WSS tunnel to relay, spawned as independent `tokio::spawn` task (uses only Send+Sync types). Proxies JSON-framed HTTP requests to local Conduwuit.
- **Push gateway client**: `push_client.rs` — HTTP client for the Symbiotic push gateway (managed mode). Sends notification metadata only (category, priority, badge), never message content.
- **Transport**: Matrix protocol via `symbiotic-matrix` crate (E2EE rooms)
- **Queue**: `symbiotic-queue` crate — job queue with workers for ingest and review pipelines
- **Event emission**: `MatrixEventEnvelope` with `org.symbiotic.event` msgtype, `sym` JSON payload
- **Config**: `DaemonConfig` struct, loaded from TOML or env vars

## Event Envelope Format

```json
{
  "msgtype": "org.symbiotic.event",
  "body": "human-readable summary",
  "sym": {
    "v": 1,
    "t": "ingest.fetch",
    "s": "running",
    "rid": "run-id-uuid",
    "ts": 1234567890,
    "p": 0.5,
    "d": { "url": "https://...", "title": "..." }
  }
}
```

## Room Roles

| Role | Purpose | Key Events |
|------|---------|------------|
| `Home` | Dashboard | `status.snapshot` (periodic) |
| `Intake` | Pipeline tracking | `intake.*`, `ingest.*`, `archive.*` |
| `Credentials` | Secrets vault | `auth.*` |
| `Goals` | Task/agent management | `goal.*`, `task.*`, `agent.*` |
| `Alerts` | Escalations only | Forwarded from other rooms on unexpected failures |

## Pipeline Flow

```
intake.started → ingest.fetch (queued/running/completed/failed)
    → archive.review.enqueue → archive.review (running/completed/failed)
```

Events carry a `rid` (run ID) threaded through the entire pipeline for correlation.

## Payload Encoding

Worker payloads use pipe-delimited format for backwards compatibility:
- **Ingest**: `record_id|queue_name|article_path|run_id`
- **Review**: `record_id=X|run_id=Y`

Decode functions handle both old (no run_id) and new formats gracefully.

## Common Commands

```bash
# Build
cargo build -p symbiotic-daemon

# Run tests
cargo test -p symbiotic-daemon

# Run all workspace tests
cargo test

# Format + lint
cargo fmt && cargo clippy

# Docker build (from repo root)
docker compose -f docker-compose.vps.yml build daemon

# Docker run full stack
docker compose -f docker-compose.vps.yml up -d

# Docker logs
docker compose -f docker-compose.vps.yml logs -f daemon
```

## Key Dependencies

- `symbiotic-core` — Core types and traits
- `symbiotic-matrix` — Matrix transport, event envelope
- `symbiotic-queue` — Job queue system
- `symbiotic-intake` — Article fetching and conversion
- `symbiotic-review` — Review pipeline
- `credential-gateway` — Secret vault

## Relay/Push Env Vars (Managed Mode)

| Variable | Required | Default | Purpose |
|----------|----------|---------|---------|
| `SYMBIOTIC_RELAY_TUNNEL_URL` | No | (disabled) | WSS URL for relay tunnel. When unset, tunnel is disabled (BYOS mode). |
| `SYMBIOTIC_RELAY_DAEMON_TOKEN` | With tunnel | — | Bearer token for tunnel + push gateway auth. |
| `SYMBIOTIC_CONDUWUIT_URL` | No | `http://localhost:8008` | Local Conduwuit URL the tunnel proxies to. |
| `SYMBIOTIC_PUSH_GATEWAY_URL` | No | (disabled) | Push gateway URL. When unset, gateway push is disabled. |

## Gotchas

- `lib.rs` was split into focused modules (commands, events, goals, install, agents) — SymbioticDaemon fields are `pub(crate)` so impl blocks can live in separate files
- Empty env vars (`VAR=`) override Rust defaults with blank strings — comment them out in config
- Conduit container has no shell — can't exec into it for debugging
- After volume wipe, rooms are recreated on daemon bootstrap — app needs reinstall (stale keys)
- Alerts are derived client-side from primary events via `_shouldNotify()` — daemon does NOT emit separate alert events (removed in Phase 2)
