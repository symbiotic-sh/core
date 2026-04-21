# Symbiotic MVP Manual Setup Guide


Step-by-step guide for deploying the Symbiotic MVP: VPS daemon, Matrix homeserver, iOS app, and X API integration.

## Prerequisites

| Requirement | Details |
|------------|---------|
| VPS | Ubuntu 22.04+ with 2GB+ RAM, Docker + Docker Compose installed |
| SSH key | At least one SSH public key installed in `authorized_keys` before hardening stage |
| Public domain | DNS hostname (for app access over HTTPS) |
| Cloudflare Tunnel | Tunnel token that routes the public hostname to the VPS Matrix service |
| Apple Developer account | For iOS device builds ($99/yr). Simulator builds work without it |
| X API credentials | Client ID + Secret from developer.x.com (for bookmarks sync) |
| Flutter SDK | 3.2.0+ on your dev machine |
| Xcode | 15+ on your Mac (for iOS builds) |
| Rust toolchain | 1.82+ (only needed if building daemon locally) |

## 1. VPS Provisioning

### 1.0 (Optional) Run one-shot install API for app-driven setup

If you want the app to provision and bootstrap a VPS directly (before Matrix exists), run the install API on your control machine/server:

```bash
cd submodules/runtime/services/symbiotic-installer
cargo run -- install-api \
  --bind 0.0.0.0:8787 \
  --root /Users/k/p/symbiotic \
  --default-repo-url <repo-url> \
  --default-repo-ref main
```

Set environment variables on the API host as needed:
- `SYMBIOTIC_INSTALL_API_TOKEN` (recommended)
- `SYMBIOTIC_HCLOUD_TOKEN` (if not passed from app)
- `SYMBIOTIC_CLOUDFLARE_TUNNEL_TOKEN` (required for public mode if not passed from app)
- `SYMBIOTIC_INSTALL_SSH_KEY` (private key path used to bootstrap remote VPS)
- `SYMBIOTIC_INSTALL_REPO_URL` (fallback repo URL)

In the app Setup screen (while disconnected), use the **One-Shot VPS Setup** card:
1. Install API URL (example: `http://<control-host>:8787`)
2. API token (if configured)
3. Public matrix domain (example: `matrix.example.com`)
4. VPS region (picker in app: `fsn1` Europe, `ash` US, `sin` APAC)
5. Hetzner token (optional if API host has `SYMBIOTIC_HCLOUD_TOKEN`)
6. Cloudflare tunnel token (optional if API host has `SYMBIOTIC_CLOUDFLARE_TUNNEL_TOKEN`)
7. Tap **One-Shot Install + Connect**

MVP one-shot provisioning uses a fixed VM profile for reliability:
- Size: `cpx42`
- Image: `ubuntu-24.04`

The app polls install status, then auto-fills homeserver/user/password and connects on success.

### 1.1 Install Docker

```bash
# On your VPS
curl -fsSL https://get.docker.com | sh
sudo usermod -aG docker $USER
# Log out and back in for group change to take effect
```

### 1.2 Configure DNS

Create a DNS hostname for your Matrix endpoint (example: `matrix.your-domain.com`) and attach it to your Cloudflare Tunnel route targeting `http://conduwuit:8008`.

```bash
# Optional verification from your machine
dig +short matrix.your-domain.com
```

### 1.3 Clone the repo

```bash
git clone <repo-url> ~/symbiotic
cd ~/symbiotic
```

## 2. Data Partition Encryption (LUKS)

Symbiotic uses LUKS2 full-disk encryption on the VPS data partition. This protects **all data at rest** (`.md` files, SQLite databases, configs, logs) against disk theft, snapshot access, and decommissioned hardware.

> **When to set this up**: Before bootstrapping services. LUKS encrypts the partition where all Symbiotic data lives (`/var/lib/symbiotic`).

### 2.0.1 Prerequisites

```bash
# Install cryptsetup if not present
sudo apt install cryptsetup
```

You need a dedicated block device (or partition) for the data. Common options:
- A second disk on the VPS (e.g., `/dev/sdb`)
- A partition on the primary disk (e.g., `/dev/sda2`)
- A Hetzner volume attached to the server

### 2.0.2 Initial setup (one-time)

```bash
cd ~/symbiotic/submodules/runtime

# Set up LUKS encryption, create filesystem, mount, and create subdirectories.
# You will be prompted to set a passphrase.
sudo ./scripts/setup-luks.sh /dev/sdb
```

The script is idempotent. It will:
1. Format the device with LUKS2 (skipped if already LUKS)
2. Open the encrypted device as `/dev/mapper/symbiotic-data`
3. Create an ext4 filesystem (skipped if already exists)
4. Mount to `/var/lib/symbiotic`
5. Create subdirectories: `data/`, `config/`, `knowledge-base/` (Archive)

### 2.0.3 After reboot

After each VPS reboot, the encrypted partition must be manually unlocked:

```bash
cd ~/symbiotic/submodules/runtime
sudo ./scripts/luks-unlock.sh /dev/sdb
# Enter your passphrase when prompted
```

Then start the Symbiotic services as normal.

### 2.0.4 Optional: add to fstab

To auto-mount after unlocking (but not auto-unlock):

```bash
echo '/dev/mapper/symbiotic-data  /var/lib/symbiotic  ext4  defaults  0  2' | sudo tee -a /etc/fstab
```

### 2.0.5 Key management options

| Method | Security | Convenience | Best for |
|--------|----------|-------------|----------|
| Passphrase at boot | High | Low (manual unlock) | Self-hosted, infrequent reboots |
| Key file on separate volume | Medium | Medium | Managed hosting |
| Network-bound (Tang/Clevis) | Medium | High (auto-unlock) | Always-on VPS |
| Vault-sealed (TPM) | High | High | Hardware with TPM |

For automated key management (Tang/Clevis, TPM), see the [LUKS documentation](https://gitlab.com/cryptsetup/cryptsetup).

## 3. Bootstrap the VPS

### 3.1 Run the bootstrap script

```bash
# Dry-run first to see what will happen
./scripts/bootstrap-vps.sh --access-mode public --public-domain matrix.your-domain.com --dry-run

# Then run for real
./scripts/bootstrap-vps.sh --access-mode public --public-domain matrix.your-domain.com
```

The bootstrap script:
- Checks prerequisites (Docker, disk space, domain config)
- Requires `SYMBIOTIC_CLOUDFLARE_TUNNEL_TOKEN` in `config/.env.secrets` (or environment)
- Configures firewall (UFW) for SSH ingress + outbound tunnel traffic
- Hardens SSH (`PasswordAuthentication no`) when at least one SSH public key exists
- Creates `config/.env.runtime` template
- Starts Docker services (`conduwuit`, `daemon`, `matrix-gateway` cloudflared tunnel)

Notes:
- Tailscale is optional and not required for phone/app connectivity in public mode.
- If you must keep password SSH login temporarily, run with `--skip-ssh-hardening`.

### 3.2 Build and start services

The daemon uses bootstrap credential resolution and room provisioning. By default, `docker-compose.vps.yml` logs in as `testuser` with password from Docker secret file `config/.secrets/matrix_password`.

```bash
docker compose -f docker-compose.vps.yml --profile public build
docker compose -f docker-compose.vps.yml --profile public up -d
```

Wait for conduwuit to become healthy:
```bash
# Check health (should return JSON with Matrix versions)
curl -f http://localhost:8008/_matrix/client/versions
```

### 3.3 Verify bootstrap

The daemon automatically:
- Logs in as `testuser` on Conduwuit (using Docker secret password)
- Ensures rooms exist: `#control`, `#intake`, `#alerts`, `#status`, `#credentials`
- Writes room IDs and bootstrap artifacts under `data/install` (or nested daemon data path)
- If `config/.secrets/matrix_password` is missing, bootstrap generates it and stores the login password in `config/.matrix-passwords`
- `config/.secrets/matrix_password` must be a single-line hex value (no wrapped newlines inside the value)
- Installer/app one-shot can supply this password explicitly via `--matrix-password-file` during bootstrap

Check daemon logs:
```bash
docker compose -f docker-compose.vps.yml --profile public logs -f daemon
# Should show: bootstrap: logged in, bootstrap: ensured room, entering idle loop
```

### 3.4 Set or rotate Matrix password (optional)

For production (and for default MVP compose), provide daemon login password via Docker secret:

```bash
# Generate a secure password
mkdir -p config/.secrets
head -c 32 /dev/urandom | xxd -p -c 256 | tr -d '\n' > config/.secrets/matrix_password
echo >> config/.secrets/matrix_password
chmod 644 config/.secrets/matrix_password

# Keep self-registration disabled in config/.env.runtime
# SYMBIOTIC_BOOTSTRAP_SELF_REGISTER=false
```

The daemon reads the password from `/run/secrets/symbiotic_matrix_password` (tmpfs, never on disk in the container).

### 3.5 Restart daemon (after config changes)

```bash
docker compose -f docker-compose.vps.yml --profile public restart daemon
```

Verify daemon is running:
```bash
docker compose -f docker-compose.vps.yml --profile public logs -f daemon
# Should show: bootstrap: logged in, entering idle loop
```

### 3.6 Verify public Matrix endpoint

In public mode, Cloudflare Tunnel terminates TLS and forwards Matrix traffic to Conduwuit.

Verify gateway:

```bash
docker compose -f docker-compose.vps.yml --profile public ps matrix-gateway
```

Verify HTTPS endpoint:

```bash
curl -f https://matrix.your-domain.com/_matrix/client/versions
```

## 4. iOS App

### 4.1 Build for simulator

```bash
cd submodules/app
flutter pub get
flutter build ios --simulator
```

### 4.2 Build for device

Requires Apple Developer account and signing:

```bash
cd submodules/app
open ios/Runner.xcworkspace
# In Xcode: set your Team in Signing & Capabilities
# Select your device, then Build & Run
```

Bundle ID: `sh.symbiotic.app`

### 4.3 Configure the app

On first launch:
1. Go to **Setup** tab
2. Enter homeserver URL: `https://matrix.your-domain.com`
3. Enter username: `testuser`
4. Enter password
5. Tap **Connect**

The connection indicator should turn green.

### 4.4 Share Extension

The share extension allows sharing URLs from Safari directly into Symbiotic:
1. Open Safari, navigate to any page
2. Tap Share > Symbiotic
3. The URL is queued and posted to `#intake` when the app resumes

Note: The share extension requires the App Group `group.sh.symbiotic.app` to be configured in your Apple Developer portal.

## 5. X API Integration

### 5.1 Get X API credentials

1. Go to [developer.x.com](https://developer.x.com)
2. Create a project and app
3. Enable OAuth 2.0 with PKCE
4. Set callback URL to match your setup
5. Request scopes: `tweet.read`, `users.read`, `bookmark.read`, `offline.access`
6. Copy Client ID and Client Secret

### 5.2 Configure credentials

Set X API credentials as environment variables in `config/.env.runtime`:
```
SYMBIOTIC_X_CLIENT_ID=your_client_id
SYMBIOTIC_X_CLIENT_SECRET=your_client_secret
```

For BYOK agent execution (goal workflows), also set:
```
ANTHROPIC_API_KEY=your_anthropic_key
OPENAI_API_KEY=your_openai_key
```

Restart daemon:
```bash
docker compose -f docker-compose.vps.yml restart daemon
```

### 5.3 Run OAuth flow

The OAuth flow is initiated via the credential gateway. When the daemon needs X API access, it posts an `auth.required` event to `#credentials`. In the app:

1. Open **Credentials** tab
2. You'll see an approval card for X OAuth
3. Tap **Approve** to start the PKCE flow
4. Complete browser-based authorization
5. Token is stored in the encrypted vault

### 5.4 Test bookmarks sync

Send a command in the **Control** tab:
```
bookmarks sync api 5
```

This queues a bookmarks sync job that:
- Fetches your 5 most recent X bookmarks
- Enqueues each URL for ingestion
- Reports progress in `#status`

## 6. Verification Checklist

| Gate | Command / Check | Expected |
|------|----------------|----------|
| Conduwuit healthy | `curl http://localhost:8008/_matrix/client/versions` | 200 with JSON |
| Public Matrix endpoint | `curl https://matrix.your-domain.com/_matrix/client/versions` | 200 with JSON |
| Docker images build | `docker compose --profile public build` | Success |
| Daemon connects | `docker compose --profile public logs daemon` | "idle" in logs |
| iOS simulator | `flutter build ios --simulator` | Build succeeds |
| App connects | Setup tab > Connect | Green indicator |
| Intake works | Intake tab > submit URL | Status event in Control tab |
| Credentials work | Credentials tab shows auth cards | Approve/deny works |
| Alerts stream | Alerts tab | Shows live alerts |
| Share extension | Share URL from Safari | Appears in intake |
| X OAuth | Complete PKCE flow | Token stored |
| Bookmarks sync | `bookmarks sync api 5` | URLs in #status |

### 6.1 Live readiness gate runner

Run strict live gates before starting the first autonomous pilot:

```bash
./scripts/live-readiness.sh --mode byok --root .
```

For managed mode:

```bash
./scripts/live-readiness.sh --mode managed --root .
```

Optional dry validation (skips network probes):

```bash
./scripts/live-readiness.sh --mode byok --root . --skip-probes
```

The script validates install artifacts from `data/install` and automatically falls back to `submodules/runtime/services/symbiotic-daemon/data/install` when daemon state is nested.
If `SYMBIOTIC_MATRIX_HOMESERVER` is set to `http://conduwuit:8008`, run with `--skip-probes` from host or use a host-reachable homeserver URL for full probe mode.

## 7. Troubleshooting

### Conduwuit won't start
- Check logs: `docker compose logs conduwuit`
- Verify `config/conduwuit.toml` is valid
- Ensure port 8008 isn't already in use

### Daemon can't connect to Matrix
- Verify conduwuit is healthy first
- If daemon runs in Docker network, `SYMBIOTIC_MATRIX_HOMESERVER=http://conduwuit:8008` is correct
- If running verifier/readiness probes from host, use a host-reachable URL (`http://127.0.0.1:8008` or `https://matrix.your-domain.com`) or `--skip-probes`
- Check daemon logs for bootstrap errors
- If using Docker secret, verify `config/.secrets/matrix_password` exists and has the correct password

### App can't reach homeserver
- Verify DNS points to Cloudflare: `dig +short matrix.your-domain.com`
- Verify gateway container is running: `docker compose --profile public ps matrix-gateway`
- Verify tunnel is connected: `docker compose --profile public logs matrix-gateway`
- Check VPS firewall allows `22/tcp` and outbound HTTPS (`443/tcp`)

### Share extension not appearing
- App Group must be configured in Apple Developer portal
- Both Runner and ShareExtension targets need the same App Group
- Rebuild after changing entitlements

### X API auth fails
- Verify Client ID and Secret are correct
- Check scopes match: `tweet.read users.read bookmark.read offline.access`
- Ensure callback URL is configured correctly in X developer portal

### Bookmarks sync returns empty
- Verify you have bookmarked tweets on X
- Check the X API token hasn't expired (2 hour TTL, auto-refreshes)
- Look at daemon logs for specific API errors
