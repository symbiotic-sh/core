# Config

Runtime configuration templates live here.

Runtime-generated files:

- `config/.env.runtime` (non-secret runtime settings)
- `config/.env.secrets.template` (reference only — secrets are injected at deploy time)
- `config/.secret_matrix_password` (Docker secret for production, optional)
- `config/systemd/symbiotic-daemon.service` (systemd unit template)

Matrix credentials (password, room IDs) are managed by daemon bootstrap and encrypted storage.
See `docs/architecture/daemon-bootstrap.md`.

## Matrix Homeserver Endpoint Rule

- `SYMBIOTIC_MATRIX_HOMESERVER=http://conduwuit:8008` is valid for daemon containers on Docker internal networking.
- Host-side checks (`scripts/live-readiness.sh`) must use a host-reachable homeserver URL.
