# BYOS Self-Hosting Guide (Bring Your Own Server)


In BYOS mode you operate everything yourself: Matrix homeserver, daemon, domain, TLS. Symbiotic operates nothing on your behalf. All code is open source.

This guide walks you through deploying the full Symbiotic stack on your own server and connecting the app in Direct mode.

## Overview

```mermaid
flowchart LR
    subgraph UserDevices["Your Devices"]
        App["Symbiotic App\n(or Element)"]
    end

    subgraph YourServer["Your Server"]
        TLS["TLS Termination\n(Caddy / nginx / Cloudflare Tunnel)"]
        Conduwuit["Conduwuit\n(Matrix homeserver)\nport 8008"]
        Daemon["Nucleus\n(symbiotic-daemon)"]
        Conduwuit <--> Daemon
    end

    subgraph Optional["Optional"]
        PushGW["push.symbiotic.sh\n(Symbiotic push gateway)"]
    end

    App -- "HTTPS" --> TLS --> Conduwuit
    Daemon -. "push notifications\n(optional)" .-> PushGW
    PushGW -. "APNs / FCM" .-> App
```

| Component | Who operates it |
|-----------|----------------|
| Matrix homeserver (Conduwuit) | You |
| Nucleus daemon | You |
| Domain + DNS + TLS | You |
| Push notifications | Your choice (see [Push Notification Options](#8-push-notification-options)) |
| Symbiotic relay | Not used (Direct mode bypasses it entirely) |

No Symbiotic infrastructure is in the critical path. The app connects directly to your Matrix server as a standard Matrix client.

---

## 1. Prerequisites

| Requirement | Details |
|------------|---------|
| Server | VPS or home machine with 2+ GB RAM, 2+ vCPU. Debian/Ubuntu recommended. |
| Docker + Docker Compose | v2+ (`docker compose` subcommand). [Install guide](https://docs.docker.com/engine/install/). |
| Domain name | Any domain you control (e.g. `matrix.example.com`). |
| Basic Linux knowledge | SSH, editing files, running commands. |
| Symbiotic source (daemon) | Clone the runtime repository for Docker build context. |

Conduwuit is lightweight (~50 MB RAM for a single-user server). The daemon adds another ~100 MB. You do not need a powerful machine.

---

## 2. Clone the Repository

```bash
git clone https://github.com/anthropic/symbiotic-runtime.git ~/symbiotic
cd ~/symbiotic
```

All paths below are relative to `~/symbiotic` (the runtime repository root).

---

## 3. Domain + DNS Setup

Point a DNS hostname to your server. This hostname is what the Symbiotic app (or Element) connects to.

**Option A: A record (direct IP)**

```
matrix.example.com  A  203.0.113.42
```

**Option B: CNAME (if behind a proxy/CDN)**

```
matrix.example.com  CNAME  your-server.provider.example
```

Verify DNS resolution from your local machine:

```bash
dig +short matrix.example.com
# Should return your server IP
```

> DNS propagation can take minutes to hours. Wait for resolution before proceeding.

---

## 4. Conduwuit Deployment (Matrix Homeserver)

Conduwuit is a lightweight Matrix homeserver written in Rust. It is the recommended homeserver for single-user Symbiotic deployments.

### 4.1 Create the Conduwuit config

Create the config directory and file:

```bash
mkdir -p config
```

Write `config/conduwuit.toml`:

```toml
[global]
# IMPORTANT: Set this to your domain (the part after the colon in Matrix user IDs).
# For example, if your Matrix IDs will be @user:example.com, set this to "example.com".
# If using a subdomain like matrix.example.com, you probably still want "example.com"
# as the server_name (with delegation), or "matrix.example.com" for simplicity.
server_name = "matrix.example.com"

# Conduwuit listens on this port inside the container.
# Do NOT change this unless you also update Docker port mappings.
port = 8008
address = "0.0.0.0"

database_path = "/var/lib/matrix-conduit"
database_backend = "rocksdb"

# Allow the first user to register. Disable after creating your account.
allow_registration = true
allow_guest_registration = false

log = "info"

# 20 MB max request size (covers media uploads)
max_request_size = 20_000_000

# Single-user server: no federation needed
allow_federation = false
trusted_servers = []
```

> **Important**: `port` must be an integer (`8008`), NOT an array (`[8008]`). The latter causes Conduwuit to fail silently.

> **Important**: After creating your user account, set `allow_registration = false` and restart Conduwuit to prevent unauthorized registrations.

### 4.2 Docker Compose for Conduwuit

We will build a single `docker-compose.yml` incrementally. Start with Conduwuit:

```yaml
# docker-compose.yml
services:
  conduwuit:
    image: matrixconduit/matrix-conduit:latest
    volumes:
      - conduwuit-data:/var/lib/matrix-conduit
      - ./config/conduwuit.toml:/etc/conduwuit.toml:ro
    environment:
      CONDUIT_CONFIG: /etc/conduwuit.toml
    ports:
      - "127.0.0.1:8008:8008"
    networks:
      - symbiotic-net
    restart: unless-stopped
    # Note: Conduwuit uses a scratch/nix-based image with no shell.
    # Exec-based healthchecks (curl, wget) cannot run inside it.

volumes:
  conduwuit-data:

networks:
  symbiotic-net:
    driver: bridge
```

Start Conduwuit and verify:

```bash
docker compose up -d conduwuit

# Wait a few seconds, then check
curl -f http://localhost:8008/_matrix/client/versions
# Should return JSON with supported Matrix versions
```

### 4.3 Create your Matrix user

While `allow_registration = true`, register your user account. You can use any Matrix client, or use curl:

```bash
curl -X POST http://localhost:8008/_matrix/client/v3/register \
  -H "Content-Type: application/json" \
  -d '{
    "username": "youruser",
    "password": "a-strong-password-here",
    "auth": { "type": "m.login.dummy" }
  }'
```

> **After registration**, edit `config/conduwuit.toml`, set `allow_registration = false`, and restart:
> ```bash
> docker compose restart conduwuit
> ```

---

## 5. TLS Setup

Matrix clients require HTTPS. Choose one of these options.

### Option A: Caddy Reverse Proxy (Recommended)

Caddy provides automatic HTTPS with zero configuration. It obtains and renews Let's Encrypt certificates automatically.

Add Caddy to your `docker-compose.yml`:

```yaml
services:
  # ... conduwuit service from above ...

  caddy:
    image: caddy:2-alpine
    ports:
      - "80:80"
      - "443:443"
    volumes:
      - ./config/Caddyfile:/etc/caddy/Caddyfile:ro
      - caddy-data:/data
      - caddy-config:/config
    networks:
      - symbiotic-net
    restart: unless-stopped
    depends_on:
      - conduwuit

volumes:
  conduwuit-data:
  caddy-data:
  caddy-config:
```

Create `config/Caddyfile`:

```
matrix.example.com {
    reverse_proxy conduwuit:8008
}
```

> Replace `matrix.example.com` with your actual domain. Caddy automatically obtains TLS certificates from Let's Encrypt.

With Caddy, **remove** the `ports` mapping from the `conduwuit` service (Caddy handles public traffic). Change the conduwuit ports line to:

```yaml
    # No public port needed — Caddy proxies traffic
    expose:
      - "8008"
```

### Option B: Cloudflare Tunnel (No Inbound Ports)

Cloudflare Tunnel creates an outbound-only connection from your server to Cloudflare's edge, so you need zero inbound ports (not even 80/443). Cloudflare handles TLS termination.

1. Create a tunnel in the [Cloudflare Zero Trust dashboard](https://one.dash.cloudflare.com/)
2. Set up a public hostname route: `matrix.example.com` pointing to `http://conduwuit:8008`
3. Copy the tunnel token

Add the tunnel to `docker-compose.yml`:

```yaml
services:
  # ... conduwuit service from above ...

  cloudflared:
    image: cloudflare/cloudflared:latest
    command: tunnel --no-autoupdate run --token ${CLOUDFLARE_TUNNEL_TOKEN}
    environment:
      - CLOUDFLARE_TUNNEL_TOKEN=${CLOUDFLARE_TUNNEL_TOKEN}
    networks:
      - symbiotic-net
    restart: unless-stopped
    depends_on:
      - conduwuit
```

Set your tunnel token in a `.env` file (same directory as `docker-compose.yml`):

```bash
# .env (do NOT commit this file)
CLOUDFLARE_TUNNEL_TOKEN=eyJ...your-token-here
```

With Cloudflare Tunnel, the `conduwuit` service does not need any `ports` mapping at all. Replace it with:

```yaml
    expose:
      - "8008"
```

### Option C: nginx + certbot (Manual ACME)

Install nginx and certbot on the host (outside Docker), or add an nginx container. This example uses host-installed nginx:

```bash
sudo apt install nginx certbot python3-certbot-nginx
```

Create `/etc/nginx/sites-available/matrix.example.com`:

```nginx
server {
    listen 80;
    server_name matrix.example.com;

    location / {
        proxy_pass http://127.0.0.1:8008;
        proxy_set_header Host $host;
        proxy_set_header X-Real-IP $remote_addr;
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto $scheme;

        # Matrix sync uses long-polling
        proxy_read_timeout 600s;

        # Media uploads
        client_max_body_size 50m;
    }
}
```

Enable and obtain certificates:

```bash
sudo ln -s /etc/nginx/sites-available/matrix.example.com /etc/nginx/sites-enabled/
sudo nginx -t && sudo systemctl reload nginx
sudo certbot --nginx -d matrix.example.com
```

Certbot will modify the config to add TLS and set up auto-renewal.

With this option, keep the `conduwuit` port mapping as `127.0.0.1:8008:8008` (nginx connects to localhost).

### Verify HTTPS

Regardless of which option you chose:

```bash
curl -f https://matrix.example.com/_matrix/client/versions
# Should return JSON with supported Matrix versions over HTTPS
```

---

## 6. Daemon Deployment

The Symbiotic daemon (Nucleus) is the orchestration service that processes commands, runs agents, and syncs data. It connects to Conduwuit over the Docker network.

### 6.1 Prepare daemon credentials

The daemon authenticates to Conduwuit using a password stored as a Docker secret. It logs in as the **same user** as the mobile app (this is required for E2EE key backup sharing).

```bash
mkdir -p config/.secrets

# Store the password you used when registering your Matrix user
echo -n 'a-strong-password-here' > config/.secrets/matrix_password
chmod 600 config/.secrets/matrix_password
```

### 6.2 Create runtime config

Create `config/.env.runtime`:

```bash
# --- Matrix connection ---
# Inside Docker network, the daemon reaches Conduwuit by service name
SYMBIOTIC_MATRIX_HOMESERVER=http://conduwuit:8008
SYMBIOTIC_MATRIX_USER=youruser

# Must match server_name in conduwuit.toml
SYMBIOTIC_MATRIX_SERVER_NAME=matrix.example.com

# --- Authorization ---
# Only this Matrix user can issue commands
SYMBIOTIC_ALLOWED_SENDERS=@youruser:matrix.example.com
SYMBIOTIC_ALLOW_OPEN_ACCESS=false

# --- Bootstrap ---
# Disable auto-registration (you already created the user)
SYMBIOTIC_BOOTSTRAP_SELF_REGISTER=false

# --- Worker settings ---
SYMBIOTIC_LOG_LEVEL=info
SYMBIOTIC_WORKER_ID=self-hosted-01
SYMBIOTIC_LEASE_SECONDS=300
```

Create `config/.env.secrets` for sensitive environment variables:

```bash
# config/.env.secrets
# Do NOT commit this file. chmod 600.

# --- AI Provider credentials (at least one required for agent execution) ---
ANTHROPIC_API_KEY=sk-ant-...
# OPENAI_API_KEY=sk-...

# --- Optional: X API credentials (for bookmarks sync) ---
# SYMBIOTIC_X_CLIENT_ID=...
# SYMBIOTIC_X_CLIENT_SECRET=...
```

```bash
chmod 600 config/.env.secrets
```

> **Important**: Do NOT leave optional vars as empty (`VAR=`). This overrides Rust defaults with blank strings. Comment them out instead.

### 6.3 Add daemon to Docker Compose

Add the daemon service and secrets to your `docker-compose.yml`:

```yaml
services:
  # ... conduwuit and TLS services from above ...

  daemon:
    build:
      context: .
      dockerfile: Dockerfile.daemon
    env_file:
      - ./config/.env.runtime
      - ./config/.env.secrets
    volumes:
      - daemon-data:/app/data
      - daemon-logs:/app/logs
    secrets:
      - symbiotic_matrix_password
    depends_on:
      conduwuit:
        condition: service_started
    networks:
      - symbiotic-net
    healthcheck:
      test: ["CMD", "symbiotic-daemon", "status"]
      interval: 30s
      timeout: 10s
      retries: 3
      start_period: 15s
    restart: unless-stopped

volumes:
  conduwuit-data:
  daemon-data:
  daemon-logs:
  # ... plus caddy volumes if using Caddy ...

secrets:
  symbiotic_matrix_password:
    file: ./config/.secrets/matrix_password
```

### 6.4 Build and start

```bash
docker compose build daemon
docker compose up -d
```

Check daemon logs:

```bash
docker compose logs -f daemon
# Should show: bootstrap: logged in, bootstrap: ensured rooms, entering idle loop
```

The daemon will:
1. Read the Matrix password from the Docker secret
2. Log in to Conduwuit as your user
3. Create rooms (`#control`, `#intake`, `#alerts`, `#status`, `#credentials`) if they don't exist
4. Enter the serve loop, waiting for commands

---

## 7. Complete Docker Compose Reference

Here is a full `docker-compose.yml` combining all services (using Caddy for TLS):

```yaml
services:
  conduwuit:
    image: matrixconduit/matrix-conduit:latest
    volumes:
      - conduwuit-data:/var/lib/matrix-conduit
      - ./config/conduwuit.toml:/etc/conduwuit.toml:ro
    environment:
      CONDUIT_CONFIG: /etc/conduwuit.toml
    expose:
      - "8008"
    networks:
      - symbiotic-net
    restart: unless-stopped

  caddy:
    image: caddy:2-alpine
    ports:
      - "80:80"
      - "443:443"
    volumes:
      - ./config/Caddyfile:/etc/caddy/Caddyfile:ro
      - caddy-data:/data
      - caddy-config:/config
    networks:
      - symbiotic-net
    restart: unless-stopped
    depends_on:
      - conduwuit

  daemon:
    build:
      context: .
      dockerfile: Dockerfile.daemon
    env_file:
      - ./config/.env.runtime
      - ./config/.env.secrets
    volumes:
      - daemon-data:/app/data
      - daemon-logs:/app/logs
    secrets:
      - symbiotic_matrix_password
    depends_on:
      conduwuit:
        condition: service_started
    networks:
      - symbiotic-net
    healthcheck:
      test: ["CMD", "symbiotic-daemon", "status"]
      interval: 30s
      timeout: 10s
      retries: 3
      start_period: 15s
    restart: unless-stopped

volumes:
  conduwuit-data:
  daemon-data:
  daemon-logs:
  caddy-data:
  caddy-config:

networks:
  symbiotic-net:
    driver: bridge

secrets:
  symbiotic_matrix_password:
    file: ./config/.secrets/matrix_password
```

---

## 8. App Configuration (Direct Mode)

The Symbiotic app supports direct Matrix connection, bypassing the Symbiotic relay entirely.

### From the App Store app

1. Open the Symbiotic app
2. Go to **Settings** (gear icon)
3. Navigate to **Advanced** section
4. Under **Connection Mode**, select **Direct**
5. Enter your homeserver URL: `https://matrix.example.com`
6. Save and return to the Setup screen
7. Enter your Matrix username and password
8. Tap **Connect**

```
Settings -> Advanced -> Connection Mode:
  ○ Managed (default) — uses relay.symbiotic.sh
  ● Direct — enter homeserver URL manually

Homeserver URL: https://matrix.example.com
```

When in Direct mode, the app connects to your server as a standard Matrix client. You can also use **Element** or any other Matrix client to connect to your homeserver.

### Same-user requirement

The app and daemon must log in as the **same Matrix user**. This is required for E2EE key backup sharing: both devices access the same Megolm session keys via SSSS (Secure Secret Storage and Sharing), so events encrypted by the daemon are decryptable by the app.

---

## 9. Push Notification Options

APNs (Apple Push Notification service) credentials are bound to the Apple Developer account that publishes the app. Since Symbiotic publishes the App Store build, only Symbiotic holds the `.p8` key. You have three options:

### Option A: Use Symbiotic's Push Gateway (Recommended)

If you are using the App Store version of the Symbiotic app, configure your daemon to call Symbiotic's push gateway. The gateway sends notification metadata only (a category like "New brief ready"), never message content.

Add to `config/.env.runtime`:

```bash
SYMBIOTIC_PUSH_GATEWAY_URL=https://push.symbiotic.sh/v1/notify
SYMBIOTIC_PUSH_DAEMON_TOKEN=dtk_...
```

> You will receive a `daemon_token` when you register your self-hosted instance with Symbiotic. This token authorizes push requests and scopes them to your devices.

**What the push gateway sees**: notification category, device token, your user ID. **What it never sees**: message content, room names, E2EE keys.

### Option B: Build the App from Source (Full Sovereignty)

If you build the Symbiotic app yourself with your own Apple Developer account and Firebase project, you can push directly without any Symbiotic infrastructure.

Add to `config/.env.secrets`:

```bash
# APNs (iOS)
SYMBIOTIC_PUSH_APNS_TEAM_ID=XXXXXXXXXX
SYMBIOTIC_PUSH_APNS_KEY_ID=YYYYYYYYYY
SYMBIOTIC_PUSH_APNS_PRIVATE_KEY=-----BEGIN PRIVATE KEY-----...

# FCM (Android, optional)
SYMBIOTIC_PUSH_FCM_PROJECT_ID=my-project
SYMBIOTIC_PUSH_FCM_SERVICE_ACCOUNT_EMAIL=...
SYMBIOTIC_PUSH_FCM_PRIVATE_KEY=-----BEGIN RSA PRIVATE KEY-----...
```

### Option C: No Push Notifications

Don't configure any push settings. The app relies on Matrix background sync for updates. Notifications arrive when the app next syncs, which may be delayed by OS background execution restrictions (iOS limits background activity).

This is acceptable for testing or if you primarily use the app in the foreground.

---

## 10. Firewall Rules

What ports to open depends on your TLS option.

### With Caddy or nginx (Options A / C)

```bash
# SSH (you probably already have this)
sudo ufw allow 22/tcp

# HTTPS for Matrix client connections
sudo ufw allow 443/tcp

# HTTP for ACME certificate challenges (Let's Encrypt)
sudo ufw allow 80/tcp

# Enable firewall
sudo ufw enable
```

### With Cloudflare Tunnel (Option B)

No inbound ports needed for Matrix traffic. Cloudflare Tunnel uses outbound connections only.

```bash
# SSH only
sudo ufw allow 22/tcp

# Deny everything else inbound (default)
sudo ufw default deny incoming
sudo ufw default allow outgoing
sudo ufw enable
```

### Summary

| Port | Protocol | Direction | Required for |
|------|----------|-----------|-------------|
| 22 | TCP | Inbound | SSH access |
| 80 | TCP | Inbound | ACME challenges (Caddy/certbot). Not needed with Cloudflare Tunnel. |
| 443 | TCP | Inbound | HTTPS Matrix traffic. Not needed with Cloudflare Tunnel. |
| 8008 | TCP | Internal only | Conduwuit. Never expose publicly; always behind reverse proxy or tunnel. |

---

## 11. Backup and Recovery

### What to back up

| Data | Location (on host) | Importance | Notes |
|------|--------------------|------------|-------|
| Conduwuit database | `conduwuit-data` Docker volume | Critical | All Matrix messages, rooms, keys |
| Daemon state | `daemon-data` Docker volume | Critical | Queue, goal artifacts, vault (encrypted credentials) |
| Daemon logs | `daemon-logs` Docker volume | Nice to have | Diagnostic only |
| Conduwuit config | `config/conduwuit.toml` | Important | Easy to recreate, but save it |
| Daemon config | `config/.env.runtime` | Important | Non-secret settings |
| Daemon secrets | `config/.env.secrets`, `config/.secrets/` | Critical | Matrix password, API keys |
| TLS state | `caddy-data` volume (if using Caddy) | Important | Certificates. Caddy auto-renews, but backup avoids rate limits. |

### Backup commands

```bash
# Stop services to ensure consistent state
docker compose stop

# Back up Docker volumes
docker run --rm \
  -v conduwuit-data:/data \
  -v $(pwd)/backups:/backup \
  alpine tar czf /backup/conduwuit-data-$(date +%Y%m%d).tar.gz -C /data .

docker run --rm \
  -v daemon-data:/data \
  -v $(pwd)/backups:/backup \
  alpine tar czf /backup/daemon-data-$(date +%Y%m%d).tar.gz -C /data .

# Back up config files
tar czf backups/config-$(date +%Y%m%d).tar.gz config/

# Restart services
docker compose up -d
```

### Restore

```bash
docker compose down

# Restore a volume (example: conduwuit)
docker volume create conduwuit-data
docker run --rm \
  -v conduwuit-data:/data \
  -v $(pwd)/backups:/backup \
  alpine tar xzf /backup/conduwuit-data-20260223.tar.gz -C /data

# Restore config
tar xzf backups/config-20260223.tar.gz

docker compose up -d
```

### Automated backups

Set up a cron job for regular backups:

```bash
# Edit crontab
crontab -e

# Add daily backup at 3 AM
0 3 * * * cd ~/symbiotic && ./scripts/backup.sh >> /var/log/symbiotic-backup.log 2>&1
```

---

## 12. Troubleshooting

### Conduwuit won't start

```bash
docker compose logs conduwuit
```

| Symptom | Cause | Fix |
|---------|-------|-----|
| Config parse error | `port` set as array `[8008]` instead of integer `8008` | Use `port = 8008` (no brackets) |
| Address already in use | Another service on port 8008 | Change the host port in Docker Compose or stop the conflicting service |
| Permission denied on database | Volume permission issue | Ensure the volume is writable by the Conduwuit process |

### Daemon can't connect to Matrix

```bash
docker compose logs daemon
```

| Symptom | Cause | Fix |
|---------|-------|-----|
| `bootstrap: failed to login` | Wrong password in Docker secret | Verify `config/.secrets/matrix_password` matches your Matrix user's password |
| `connection refused` to conduwuit | Conduwuit not running or wrong URL | Check `SYMBIOTIC_MATRIX_HOMESERVER=http://conduwuit:8008` and that both services are on the same Docker network |
| `M_FORBIDDEN` on registration | `allow_registration = false` | You already registered. Set `SYMBIOTIC_BOOTSTRAP_SELF_REGISTER=false` and provide the password via Docker secret. |

### App can't reach homeserver

| Symptom | Cause | Fix |
|---------|-------|-----|
| Connection timeout | DNS not resolving | `dig +short matrix.example.com` should return your server IP |
| Certificate error | TLS not set up / cert expired | Verify HTTPS works: `curl -f https://matrix.example.com/_matrix/client/versions` |
| 502 Bad Gateway | Reverse proxy can't reach Conduwuit | Check that Conduwuit is running and the proxy config points to the correct address |
| App shows "Managed mode" | Not in Direct mode | Settings -> Advanced -> Connection Mode -> Direct |

### E2EE issues

| Symptom | Cause | Fix |
|---------|-------|-----|
| Can't decrypt daemon messages | App and daemon on different users | Both MUST log in as the same Matrix user |
| Key backup not working | SSSS not initialized | Let the app bootstrap SSSS first, then start the daemon |
| `initCryptoIdentity` fails | Stale SSSS data | Uninstall the app, delete Conduwuit data, and start fresh |

### Docker build fails

| Symptom | Cause | Fix |
|---------|-------|-----|
| Rust compilation error | Wrong toolchain version | Dockerfile uses `rust:1.88-bookworm`. Ensure Docker has enough RAM (4+ GB) for compilation. |
| `libsqlite3-0` not found | Stale apt cache in Docker | The Dockerfile clears apt lists before updating. If building fails, try `docker build --no-cache`. |

### General diagnostics

```bash
# Check all services
docker compose ps

# Check Conduwuit health
curl -f http://localhost:8008/_matrix/client/versions

# Check HTTPS endpoint
curl -f https://matrix.example.com/_matrix/client/versions

# Follow daemon logs
docker compose logs -f daemon

# Check disk space (Conduwuit database grows over time)
df -h
docker system df
```

---

## 13. Upgrading

### Update Conduwuit

```bash
docker compose pull conduwuit
docker compose up -d conduwuit
```

### Update the daemon

```bash
cd ~/symbiotic
git pull
docker compose build daemon
docker compose up -d daemon
```

### Update everything

```bash
cd ~/symbiotic
git pull
docker compose pull
docker compose build
docker compose up -d
```

> Always back up before upgrading. Conduwuit database migrations are generally automatic, but breaking changes are possible between major versions.

---

## Appendix A: Full Setup Checklist

Use this to verify your deployment end to end.

| Step | Command / Action | Expected Result |
|------|-----------------|-----------------|
| DNS resolves | `dig +short matrix.example.com` | Your server's IP |
| Conduwuit responds | `curl http://localhost:8008/_matrix/client/versions` | JSON with versions |
| HTTPS works | `curl https://matrix.example.com/_matrix/client/versions` | JSON over HTTPS |
| User registered | Matrix login succeeds | Access token returned |
| Registration disabled | `allow_registration = false` in conduwuit.toml | New registrations fail |
| Daemon boots | `docker compose logs daemon` | "entering idle loop" |
| App connects (Direct) | Settings -> Direct -> Connect | Green connection indicator |
| E2EE works | Send message from app | Daemon processes it |
| Rooms created | Check daemon logs | `#control`, `#intake`, etc. |

## Appendix B: BYOS vs Managed Mode Comparison

```mermaid
flowchart TB
    subgraph Managed["Managed Mode (Default)"]
        MA[App] -->|"HTTPS via relay"| MR[relay.symbiotic.sh]
        MR -->|"WSS tunnel"| MD[Daemon + Conduwuit\non your VPS]
        MD2[Daemon] -->|"push metadata"| MP[push.symbiotic.sh]
        MP -->|"APNs/FCM"| MA
    end

    subgraph BYOS["BYOS Mode (This Guide)"]
        BA[App] -->|"direct HTTPS"| BC[Your Conduwuit\n+ TLS]
        BD[Daemon] <-->|"localhost"| BC
        BD -.->|"optional"| BP[push.symbiotic.sh\nor your own APNs]
        BP -.->|"APNs/FCM"| BA
    end
```

| Aspect | Managed | BYOS |
|--------|---------|------|
| Domain required | No (relay provides URL) | Yes |
| TLS setup | Handled by relay | You manage it |
| Push notifications | Automatic via gateway | Choose: gateway, self-build, or none |
| Inbound ports | None (daemon connects outbound) | 443 (or none with Cloudflare Tunnel) |
| Symbiotic infrastructure used | Relay + Push Gateway | None (or Push Gateway only) |
| Any Matrix client works | Yes (through relay) | Yes (direct connection) |
| Data sovereignty | E2EE (relay sees HTTP headers only) | Full (no third-party infrastructure) |
