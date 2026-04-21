# Connectivity Architecture: Relay + Push Gateway


**Status**: Draft v2 — revised to HTTP reverse proxy model
**Task**: New (supersedes DNS/ACME dependency for MVP)
**Priority**: P0
**Links**: `docs/architecture/vps-deployment.md`, `docs/architecture/matrix-channels.md`

## Problem

The current architecture requires every user to:
1. Purchase a domain
2. Configure DNS records
3. Set up a Cloudflare Tunnel (or Tailscale on every device)
4. Manage TLS certificates

This is a massive onboarding barrier. Most users will never complete it.

Additionally, push notifications (APNs/FCM) require credentials tied to the Apple Developer / Firebase account that **publishes the app**. Self-hosted daemon operators cannot push to Symbiotic's published app without Symbiotic's `.p8` key — and distributing that key is a non-starter.

## Solution

Two connectivity modes, both first-class:

| Mode | Who it's for | What Symbiotic operates | What user operates |
|------|-------------|------------------------|-------------------|
| **Managed** (default) | Everyone | Relay + Push Gateway | Daemon (on their VPS/machine) |
| **BYOS** | Power users / sovereignty | Nothing | Everything (Matrix server, domain, DNS, optionally their own app build) |

Symbiotic's value proposition is **the app + the seamless setup**, not infrastructure lock-in. All code is open. BYOS is fully documented.

## Architecture: Managed Mode

```mermaid
flowchart LR
    subgraph UserDevices["User Devices"]
        iPhone["App / Element\niPhone"]
        iPad["App / Element\niPad"]
    end

    subgraph Symbiotic["Symbiotic Cloud (control-plane)"]
        Relay["Relay\nrelay.symbiotic.sh\n─────────────\nHTTP reverse proxy\nRoutes by slug\nSees: HTTP headers only"]
        Push["Push Gateway\npush.symbiotic.sh\n─────────────\nHolds APNs .p8\nHolds FCM SA key"]
    end

    subgraph UserVPS["User's VPS / Machine"]
        Tunnel["Tunnel Client\n(outbound WSS)"]
        Daemon["Nucleus\n(symbiotic-daemon)"]
        Conduwuit["Conduwuit\n(per-user Matrix)"]
        Tunnel <--> Conduwuit
        Daemon <--> Conduwuit
    end

    subgraph Apple["Apple / Google"]
        APNs[APNs]
        FCM[FCM]
    end

    iPhone -- "standard HTTPS\n/_matrix/client/*" --> Relay
    iPad -- "standard HTTPS\n/_matrix/client/*" --> Relay
    Relay -- "HTTP-over-WSS tunnel" --> Tunnel

    Daemon -- "HTTPS\nmetadata only" --> Push
    Push --> APNs --> iPhone
    Push --> FCM
```

### Key properties:
- **Standard Matrix HTTP API** — the app (or Element, or any Matrix client) speaks normal `/_matrix/client/v3/*` to the relay. Zero custom transport code on the app side.
- **Per-user Conduwuit** — each daemon runs its own Matrix server locally. No shared Matrix state.
- **Relay is an HTTP reverse proxy** — routes requests to the correct daemon by slug, forwards responses back. Sees HTTP headers but not E2EE message content.
- **Daemon connects outbound** — persistent WebSocket tunnel from daemon to relay. No inbound ports, no DNS, no domain needed.
- **Push gateway holds credentials** — APNs `.p8` + FCM service account. Never distributed.
- **Element compatible** — any standard Matrix client can connect through the relay.

## Component 1: Relay Service (`relay.symbiotic.sh`)

### What it is

An HTTP reverse proxy that routes standard Matrix Client-Server API requests to per-user daemons. Each user gets an opaque routing slug. The daemon maintains a persistent outbound WebSocket tunnel to the relay.

### Routing slugs

Each user gets a random, unguessable slug during onboarding:

```
https://relay.symbiotic.sh/p/quiet-forest-42/_matrix/client/v3/sync
https://relay.symbiotic.sh/p/quiet-forest-42/_matrix/client/v3/rooms/!abc:local/send/m.room.message/1
```

The slug is a **routing identifier, not a secret**:
- Knowing the slug tells you where a Conduwuit lives, but not how to log in
- Matrix authentication (username/password → access token) handles actual auth
- Slugs are random and unguessable (e.g. 3 random words + 2 digits) so they can't be enumerated
- The app sets its homeserver URL to `https://relay.symbiotic.sh/p/<slug>`
- The Matrix SDK appends `/_matrix/client/v3/...` paths as normal

### Protocol

```mermaid
sequenceDiagram
    participant D as Daemon (VPS)
    participant R as Relay (relay.symbiotic.sh)
    participant A as App / Element

    Note over D,R: Daemon establishes persistent tunnel
    D->>R: WSS connect to /v1/daemon-tunnel<br/>Authorization: Bearer <daemon_token>
    R->>R: Authenticate → user_id → slug mapping
    R-->>D: Tunnel established

    Note over A,R: App uses standard Matrix HTTP API
    A->>R: GET /p/quiet-forest-42/_matrix/client/v3/sync<br/>Authorization: Bearer <matrix_access_token>
    R->>R: Strip /p/quiet-forest-42 prefix<br/>Look up slug → daemon tunnel
    R->>D: Forward: GET /_matrix/client/v3/sync<br/>(over WSS tunnel)
    D->>D: Proxy to local Conduwuit
    D-->>R: HTTP response (encrypted sync data)
    R-->>A: HTTP response (passed through)

    Note over A,R: Long-poll sync works naturally
    A->>R: GET /_matrix/client/v3/sync?timeout=30000
    R->>D: Forward (over tunnel)
    Note over D: Conduwuit holds connection<br/>until new events
    D-->>R: Response with new events
    R-->>A: Response (E2EE encrypted)

    Note over D,R: Daemon goes offline
    D--xR: Tunnel disconnects
    A->>R: GET /p/quiet-forest-42/_matrix/client/v3/sync
    R-->>A: 502 Bad Gateway
    Note over A: Matrix SDK retries<br/>with built-in backoff
```

### Why HTTP reverse proxy (not custom WebSocket)

| Aspect | HTTP reverse proxy | Custom WebSocket pipe (v1, superseded) |
|--------|-------------------|---------------------------------------|
| Element compatible | Yes | No |
| Any Matrix client | Yes | No |
| Offline handling | Matrix SDK's built-in retry/backoff | Hand-rolled app-side queue |
| App-side transport code | Zero (just set homeserver URL) | Custom WebSocket wrapper |
| Long-poll sync | Works naturally | Needs special handling |
| Complexity | Low app-side, moderate daemon-side | High both sides |

### What the relay sees

| Data | Visible? | Stored? |
|------|----------|---------|
| HTTP headers (path, method, Content-Length) | Yes | Access logs only |
| Matrix access token | Passes through (not validated by relay) | No |
| E2EE message content | No (encrypted in request/response body) | No |
| Room names, membership | No (encrypted or opaque room IDs) | No |
| Routing slug | Yes (needed for routing) | In-memory routing table |
| Client IP address | Yes | Logs only (rotation policy) |

### What the relay does NOT do

- Validate Matrix access tokens (Conduwuit does that)
- Parse, inspect, or store HTTP request/response bodies
- Buffer requests when daemon is offline (returns 502 immediately)
- Maintain message history or queues
- Understand Matrix protocol semantics

### Daemon tunnel protocol

The daemon maintains a persistent outbound WebSocket to the relay:

```
wss://relay.symbiotic.sh/v1/daemon-tunnel
Authorization: Bearer <daemon_token>
```

HTTP requests from clients are forwarded through this tunnel as framed messages:

```
Relay → Daemon (request frame):
{
  "id": "req_001",
  "method": "GET",
  "path": "/_matrix/client/v3/sync?timeout=30000",
  "headers": { "authorization": "Bearer syt_...", "content-type": "..." },
  "body": null
}

Daemon → Relay (response frame):
{
  "id": "req_001",
  "status": 200,
  "headers": { "content-type": "application/json" },
  "body": "<raw response bytes>"
}
```

The daemon receives these frames, proxies them to local Conduwuit via `http://localhost:8008`, and sends the response back. Multiple requests can be in-flight concurrently (multiplexed by `id`).

### Offline behavior

When the daemon tunnel is not connected, the relay returns **502 Bad Gateway** for all requests to that slug. The Matrix SDK handles this natively:

- SDK retries with exponential backoff (built into every Matrix client)
- Outgoing messages are queued in the SDK's local store
- When the daemon reconnects, the next `/sync` picks up where it left off
- No custom offline logic needed in the app

### Multi-device support

Multiple devices (iPhone, iPad, desktop) all set the same homeserver URL (`https://relay.symbiotic.sh/p/<slug>`). Each device has its own Matrix session (access token, device ID, E2EE keys). The relay forwards all their requests through the same daemon tunnel. This is standard Matrix multi-device — no special handling needed.

### Slug provisioning

```
POST /v1/relay/provision
Authorization: Bearer <control_plane_token>
{
  "user_id": "usr_abc123"
}
→ {
  "slug": "quiet-forest-42",
  "homeserver_url": "https://relay.symbiotic.sh/p/quiet-forest-42",
  "daemon_token": "dtk_...",
  "daemon_tunnel_url": "wss://relay.symbiotic.sh/v1/daemon-tunnel"
}
```

- **slug**: Random 3-word + 2-digit identifier (e.g. `quiet-forest-42`, `bold-river-17`). Used in the homeserver URL. Not a secret — just unguessable.
- **daemon_token**: Used by the daemon to authenticate its tunnel connection. This IS a secret (long-lived Bearer token). Stored in daemon config, never in URLs.
- **homeserver_url**: What the app sets as its Matrix homeserver. Shareable — knowing it doesn't grant access.

### Scaling

- Stateless HTTP proxy — horizontal scale behind a load balancer
- Sticky routing by slug (all requests for one slug go to the relay instance holding that daemon's tunnel)
- Each daemon tunnel is independent — no cross-user state
- Estimated: 10K concurrent daemon tunnels on a single 2-core instance

### Failure modes

| Failure | Impact | Recovery |
|---------|--------|----------|
| Relay restart | All daemon tunnels drop, apps get 502 | Daemons auto-reconnect (exponential backoff) |
| Relay overload | New connections/requests rejected | Horizontal scale, rate limiting |
| Daemon offline | Apps get 502 for that slug | Matrix SDK retries; daemon reconnects when available |
| Invalid slug | 404 Not Found | User re-provisions via control plane |
| Invalid daemon token | Tunnel rejected (401) | User re-provisions via control plane |

## Component 2: Push Gateway (`push.symbiotic.sh`)

### What it is

An HTTPS endpoint that accepts push requests from authenticated daemons and forwards them to APNs/FCM using Symbiotic's credentials. Sends **notification metadata only** — never message content.

### Why it's required

APNs credentials (`.p8` key) are bound to the Apple Developer account that publishes the app. FCM credentials (service account) are bound to the Firebase project. Since Symbiotic publishes the app, only Symbiotic can hold these credentials. Self-hosted daemons call the gateway instead of calling APNs/FCM directly.

### Protocol

```
POST https://push.symbiotic.sh/v1/notify
Authorization: Bearer <daemon_token>
Content-Type: application/json

{
  "device_id": "iphone-abc",
  "platform": "apns",
  "device_token_encrypted": "<encrypted APNs device token>",
  "notification": {
    "category": "brief_ready",
    "priority": "normal",
    "badge": 3
  }
}
```

### What the gateway sees

| Data | Visible? |
|------|----------|
| Notification category | Yes (needed for APNs/FCM payload) |
| Message content | No (never sent — no field for it) |
| Device token | Yes (needed to address the push) |
| User identity | Yes (from daemon token) |

### What the gateway does NOT see

- E2EE message content (never leaves daemon/app boundary)
- Room names, membership, or any Matrix metadata
- Vault secrets, credentials, capability tokens

### Security constraints

- **Rate limited** per user (prevent abuse of APNs/FCM quotas)
- **Token scoping** — daemon can only push to devices registered under its own user_id
- **No content field** — API schema does not accept a `body` or `content` field. Notification titles are derived from a fixed category enum on the gateway side, not user-supplied.
- **Audit log** — all push requests logged (user_id, device_id, category, timestamp). No content to log.

### Notification categories (fixed set)

```rust
enum NotificationCategory {
    BriefReady,         // "New brief ready"
    GoalProgress,       // "Goal update"
    GoalCompleted,      // "Goal completed"
    ActionRequired,     // "Action needed"
    EscalationPending,  // "Approval requested"
    SecurityAlert,      // "Security alert"
    SystemStatus,       // "System update"
}
```

Titles are derived from the category on the gateway side — the daemon does not supply freeform notification text. This prevents the push channel from becoming a content exfiltration vector.

## Component 3: BYOS Mode (Bring Your Own Server)

### Overview

BYOS users run everything themselves. Symbiotic provides documentation and tooling but operates nothing. The daemon code, Conduwuit config, and all infrastructure scripts are open source.

```mermaid
flowchart LR
    subgraph UserDevices["User Devices"]
        App["App / Element\n(direct Matrix client)"]
    end

    subgraph UserInfra["User's Infrastructure"]
        Domain["user's domain\n+ DNS + TLS"]
        Conduwuit["Conduwuit"]
        Daemon["Nucleus"]
        Conduwuit <--> Daemon
    end

    subgraph Optional["Optional: Symbiotic Push"]
        PushGW["push.symbiotic.sh"]
    end

    App -- "direct HTTPS\n(user's domain)" --> Domain --> Conduwuit
    Daemon -. "optional" .-> PushGW
    PushGW -. "APNs / FCM" .-> App
```

No Symbiotic infrastructure in the critical path. Push gateway is optional (can use direct APNs/FCM creds if self-building the app).

### What BYOS users operate

| Component | What they provide |
|-----------|------------------|
| Matrix server | Their own Conduwuit (or any Matrix homeserver) |
| Connectivity | Their own domain + DNS + Cloudflare Tunnel / Tailscale / direct |
| TLS | Their own certs (manual or ACME) |
| Push (option A) | Use `push.symbiotic.sh` gateway (if using Symbiotic's published app) |
| Push (option B) | Build the app themselves + use their own APNs/FCM creds |
| Push (option C) | No push — rely on Matrix background sync only |

### App configuration for BYOS

The app supports direct Matrix connection (no relay):

```
Settings → Advanced → Connection Mode:
  ○ Managed (default) — uses relay.symbiotic.sh
  ● Direct — enter homeserver URL manually

Homeserver URL: https://matrix.example.com
```

When in Direct mode, the app connects to the user's Matrix server as a standard Matrix client. The relay is not involved. Element can also be used — it's just a Matrix homeserver URL.

### Push in BYOS mode

**Option A (recommended for most BYOS users):** Use Symbiotic's published app + `push.symbiotic.sh`. The daemon is configured with:

```env
SYMBIOTIC_PUSH_GATEWAY_URL=https://push.symbiotic.sh/v1/notify
SYMBIOTIC_PUSH_DAEMON_TOKEN=dtk_...
```

The daemon calls the push gateway for notifications. User gets push on the standard App Store app.

**Option B (full sovereignty):** Build the app from source with your own Apple Developer account + Firebase project. Configure the daemon with direct APNs/FCM credentials:

```env
SYMBIOTIC_PUSH_APNS_TEAM_ID=XXXXXXXXXX
SYMBIOTIC_PUSH_APNS_KEY_ID=YYYYYYYYYY
SYMBIOTIC_PUSH_APNS_PRIVATE_KEY=-----BEGIN PRIVATE KEY-----...
SYMBIOTIC_PUSH_FCM_PROJECT_ID=my-project
SYMBIOTIC_PUSH_FCM_SERVICE_ACCOUNT_EMAIL=...
SYMBIOTIC_PUSH_FCM_PRIVATE_KEY=-----BEGIN RSA PRIVATE KEY-----...
```

**Option C (no push):** Don't configure any push. The app relies on Matrix background sync for updates. Notifications arrive when the app next syncs (may be delayed by OS background restrictions).

### BYOS documentation requirements

A self-hosting guide must cover:
1. Domain + DNS setup (A record or CNAME)
2. Conduwuit deployment (Docker or binary)
3. TLS options (ACME via control-plane, manual certs, or Cloudflare Tunnel)
4. Daemon deployment (Docker Compose)
5. App configuration (direct mode, homeserver URL)
6. Push options (gateway vs direct vs none)
7. Firewall rules (inbound HTTPS for Matrix, or tunnel-only)
8. Backup and recovery

## Onboarding Flow Changes

### Managed mode (default)

```mermaid
sequenceDiagram
    participant U as User
    participant A as App
    participant CP as Control Plane
    participant R as Relay
    participant D as Daemon (VPS)

    U->>A: Install from App Store
    A->>CP: Create account
    CP->>CP: Provision slug + daemon token
    CP-->>A: slug, homeserver_url, daemon_token

    U->>D: Start daemon (with daemon_token)
    D->>R: WSS tunnel connect
    R-->>D: Tunnel established

    A->>A: Set homeserver = relay.symbiotic.sh/p/<slug>
    A->>R: Matrix login (/_matrix/client/v3/login)
    R->>D: Forward to Conduwuit
    D-->>R: Login success + access_token
    R-->>A: Login success

    Note over A,D: App ↔ Daemon communicating via standard Matrix API
    A->>R: /sync, /send, etc.
    R->>D: Forwarded through tunnel
    D-->>R: Responses
    R-->>A: Responses
```

### BYOS mode

```
1. User installs app (App Store or self-built)
2. User deploys Conduwuit + daemon (docs/SELF-HOSTING.md)
3. User configures domain + DNS + TLS
4. User opens app → Settings → Direct mode → enters homeserver URL
5. App connects directly to user's Matrix server
6. Setup wizard completes
7. Push: user configures gateway URL or direct creds (optional)
```

## Migration Path

### From current architecture

The current Cloudflare Tunnel approach becomes a subset of BYOS mode. Users already using it keep working — nothing breaks. The relay is additive.

| Current | New |
|---------|-----|
| Cloudflare Tunnel (requires domain) | Relay (no domain) OR Cloudflare Tunnel (BYOS) |
| Direct APNs/FCM (requires creds in daemon) | Push gateway (managed) OR direct creds (BYOS) |
| One connectivity mode | Two modes: Managed + BYOS |

### Implementation order

1. **Relay HTTP proxy + daemon tunnel** — new module in control-plane (`src/relay/`)
2. **Push gateway endpoint** — new route in control-plane API (`/v1/notify`)
3. **Daemon: tunnel client** — outbound WSS connection + HTTP request proxying to local Conduwuit
4. **App: homeserver URL config** — just set URL to `relay.symbiotic.sh/p/<slug>` (no custom transport)
5. **App: connection mode toggle** — Settings → Managed/Direct
6. **Onboarding flow** — provision slugs + daemon tokens during account creation
7. **BYOS documentation** — `docs/SELF-HOSTING.md`

The relay and push gateway live in the control-plane codebase — it already has TLS, rate limiting, auth middleware, and the provisioning API. No new service to deploy.

### What changes from v1 implementation

The Phase 1 relay code (custom WebSocket pipe) needs to be **replaced** with an HTTP reverse proxy model. The existing `relay.rs` WebSocket infrastructure can be reused for the daemon tunnel side — but the app-facing side changes from WebSocket to HTTP.

Specifically:
- **Keep**: `RelayState` routing table, token provisioning/revocation, daemon WebSocket connection handling
- **Replace**: App-side WebSocket handlers → HTTP reverse proxy handler
- **Remove**: Frame protocol (0x01/0x02/0x03), app-side WebSocket, relay_status signaling, app-side queue design
- **Add**: HTTP request forwarding through daemon tunnel, slug-based routing, request/response framing over the daemon WebSocket

## Security Model Summary

| Threat | Managed mode mitigation | BYOS mode mitigation |
|--------|------------------------|---------------------|
| Cross-user data leak | No shared state (per-user Conduwuit, slug isolation) | N/A (single user) |
| Relay sees content | E2EE — relay sees HTTP headers but not decrypted bodies | N/A (no relay) |
| Slug enumeration | Random unguessable slugs (3 words + 2 digits) | N/A |
| Push content leak | Fixed notification categories, no content field | User controls push config |
| Relay compromise | Attacker sees HTTP metadata + encrypted bodies. No Matrix access tokens stored (passed through). | N/A |
| Push gateway compromise | Attacker can send spam notifications (no content access) | N/A if using direct creds |
| Credential distribution | APNs/FCM creds never leave gateway | User holds own creds |
| Token in URL | Slug is a routing ID, not a secret. Matrix auth is separate. | N/A |

## Resolved Decisions

1. **Offline handling** — Relay returns 502 when daemon is offline. Matrix SDK handles retry with built-in exponential backoff. No custom client-side queue. No relay_status signaling. Standard HTTP behavior.

2. **Multiple devices** — All devices use the same homeserver URL (same slug). Each has its own Matrix session/access token/device keys. Standard Matrix multi-device. No special handling.

3. **Where relay lives** — In the control-plane codebase (`src/relay/`). Already has TLS, rate limiting, auth. No new service to deploy or operate.

4. **Slug format** — Random 3-word + 2-digit slug (e.g. `quiet-forest-42`). Not a secret — just an unguessable routing ID. Matrix credentials handle actual authentication. No tokens in URLs.

5. **Relay token lifetime** — Daemon token is long-lived, tied to account. Daemon runs 24/7 unattended. Rotatable/revocable via control plane.

6. **Relay location** — Single region for MVP. Matrix sync is tolerant of 100-200ms latency. Revisit for multi-region when users span continents.

7. **Push gateway SLA** — Best-effort. APNs/FCM are themselves best-effort. Push is a wakeup signal. Matrix sync delivers actual content.

## Resolved: Remaining Open Questions

**8. Slug collision** — Retry-on-collision loop during generation: `while slugs.contains(candidate) { regenerate }`. With ~800B combinations and at most thousands of users, collisions are astronomically rare, but the check costs nothing.

**9. Daemon tunnel keepalive** — 30 seconds. AWS ALB idles at 60s, most NATs at 60-120s. 30s gives comfortable margin. Relay sends WebSocket ping, daemon responds with pong. If 3 consecutive pings get no pong, relay drops the tunnel and marks slug as offline (502 for subsequent requests).

**10. Request size limits** — 50 MB passthrough limit. Matrix content repo (`/_matrix/media/`) handles file uploads; Conduwuit defaults to 20MB for media. 50MB gives headroom without enabling abuse. The relay streams chunks through the tunnel rather than buffering the full body in memory.
