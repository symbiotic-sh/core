#!/usr/bin/env bash
# setup-matrix-rooms.sh - Ensure Matrix app user + rooms for Symbiotic MVP.
#
# Called by bootstrap-vps.sh after conduwuit is healthy.
# Idempotent: skips creation if users/rooms already exist.
#
# Usage: setup-matrix-rooms.sh [--homeserver URL] [--password-file PATH] [--dry-run]

set -euo pipefail

# ---------------------------------------------------------------------------
# Constants
# ---------------------------------------------------------------------------
ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CONFIG_DIR="$ROOT_DIR/config"
ROOMS_ENV_FILE="$CONFIG_DIR/.env.rooms"
LOG_FILE="$ROOT_DIR/.bootstrap.log"

DEFAULT_HOMESERVER="http://localhost:8008"
DEFAULT_SERVER_NAME="symbiotic.local"
HEALTH_TIMEOUT_S=60
HEALTH_INTERVAL_S=5
CURL_CONNECT_TIMEOUT_S=3
CURL_MAX_TIME_S=10

APP_USER_DEFAULT="testuser"
APP_DISPLAY="Symbiotic User"

ROOM_ALIASES=("control" "intake" "alerts" "status" "credentials" "goals")

# ---------------------------------------------------------------------------
# Globals
# ---------------------------------------------------------------------------
HOMESERVER="$DEFAULT_HOMESERVER"
SERVER_NAME="$DEFAULT_SERVER_NAME"
DRY_RUN=false
APP_USER="$APP_USER_DEFAULT"
APP_TOKEN=""
# Initialize with a no-op flag to avoid bash 3.2 empty-array + set -u bug.
# -s (silent) is harmless and ensures the array is never empty.
CURL_TLS_ARGS=("-s")
PASSWORD_FILE=""

# ---------------------------------------------------------------------------
# Logging (matches bootstrap-vps.sh style)
# ---------------------------------------------------------------------------
log() {
  local level="$1"; shift
  local msg
  msg="[$(date -u '+%Y-%m-%dT%H:%M:%SZ')] [$level] [room-setup] $*"
  echo "$msg" | tee -a "$LOG_FILE"
}

info()  { log "INFO"  "$@"; }
warn()  { log "WARN"  "$@"; }
error() { log "ERROR" "$@"; }

# ---------------------------------------------------------------------------
# Argument parsing
# ---------------------------------------------------------------------------
parse_args() {
  while [[ $# -gt 0 ]]; do
    case "$1" in
      --homeserver)  shift; HOMESERVER="$1" ;;
      --server-name) shift; SERVER_NAME="$1" ;;
      --insecure)    CURL_TLS_ARGS+=("-k") ;;
      --password-file) shift; PASSWORD_FILE="$1" ;;
      --dry-run)     DRY_RUN=true ;;
      -h|--help)     usage; exit 0 ;;
      *)             error "Unknown flag: $1"; usage; exit 1 ;;
    esac
    shift
  done
}

usage() {
  cat <<'EOF'
Usage: setup-matrix-rooms.sh [OPTIONS]

Options:
  --homeserver URL    Matrix homeserver URL (default: http://localhost:8008)
  --server-name NAME  Matrix server name (default: symbiotic.local)
  --insecure          Disable TLS verification for bootstrap calls
  --password-file PATH  Use app-user password from file instead of auto-generated value
  --dry-run           Print actions without executing
  -h, --help          Show this help
EOF
}

# ---------------------------------------------------------------------------
# Generate a random password (32 hex chars)
# ---------------------------------------------------------------------------
generate_password() {
  head -c 16 /dev/urandom | xxd -p -c 256 | tr -d '\n'
}

validate_password() {
  local value="$1"
  if [[ -z "$value" ]]; then
    error "Matrix app-user password is empty"
    return 1
  fi
  if [[ "$value" =~ [[:space:]] ]]; then
    error "Matrix app-user password must be a single-line value"
    return 1
  fi
  if [[ "${#value}" -lt 16 ]]; then
    error "Matrix app-user password must be at least 16 characters"
    return 1
  fi
}

# ---------------------------------------------------------------------------
# Wait for conduwuit to be healthy
# ---------------------------------------------------------------------------
wait_for_homeserver() {
  info "Waiting for homeserver at $HOMESERVER ..."
  local elapsed=0

  while [[ "$elapsed" -lt "$HEALTH_TIMEOUT_S" ]]; do
    if curl "${CURL_TLS_ARGS[@]}" -sf --connect-timeout "$CURL_CONNECT_TIMEOUT_S" --max-time "$CURL_MAX_TIME_S" \
      "$HOMESERVER/_matrix/client/versions" >/dev/null 2>&1; then
      info "Homeserver is healthy"
      return 0
    fi
    sleep "$HEALTH_INTERVAL_S"
    elapsed=$((elapsed + HEALTH_INTERVAL_S))
  done

  error "Homeserver not healthy after ${HEALTH_TIMEOUT_S}s"
  return 1
}

# ---------------------------------------------------------------------------
# Load runtime Matrix user from config/.env.runtime when present.
# ---------------------------------------------------------------------------
load_runtime_user() {
  local runtime_env="$CONFIG_DIR/.env.runtime"
  if [[ -f "$runtime_env" ]] && ! $DRY_RUN; then
    # shellcheck disable=SC1090
    source "$runtime_env"
    if [[ -n "${SYMBIOTIC_MATRIX_USER:-}" ]]; then
      APP_USER="$SYMBIOTIC_MATRIX_USER"
    fi
  fi
}

# ---------------------------------------------------------------------------
# Resolve app-user password with priority:
# 1) Docker secret file config/.secrets/matrix_password
# 2) Legacy/local password record config/.matrix-passwords
# 3) Generate new password and persist to both files
# ---------------------------------------------------------------------------
load_app_password() {
  local password_file="$CONFIG_DIR/.matrix-passwords"
  local secret_file="$CONFIG_DIR/.secrets/matrix_password"
  local password=""

  if [[ -n "$PASSWORD_FILE" ]]; then
    if [[ ! -f "$PASSWORD_FILE" ]]; then
      error "Password file not found: $PASSWORD_FILE"
      return 1
    fi
    password=$(tr -d '\r\n' < "$PASSWORD_FILE")
  fi

  if [[ -f "$secret_file" ]] && ! $DRY_RUN; then
    if [[ -z "$password" ]]; then
      password=$(tr -d '\r\n' < "$secret_file")
    fi
  fi

  if [[ -z "$password" ]] && [[ -f "$password_file" ]] && ! $DRY_RUN; then
    # shellcheck disable=SC1090
    source "$password_file"
    password="${MATRIX_TESTUSER_PASSWORD:-}"
    if [[ -z "$password" ]]; then
      password="${MATRIX_DAEMON_PASSWORD:-}"
    fi
    password=$(echo "$password" | tr -d '\r\n')
  fi

  if [[ -z "$password" ]]; then
    password=$(generate_password)
  fi
  validate_password "$password"

  if ! $DRY_RUN; then
    mkdir -p "$CONFIG_DIR/.secrets"
    printf "%s\n" "$password" > "$secret_file"
    # Docker Compose file-based secrets are bind-mounted as-is in this setup.
    # Use 0644 so the non-root daemon user can read /run/secrets/... in-container.
    chmod 644 "$secret_file"

    cat > "$password_file" <<PWEOF
# Matrix app-user password -- NEVER commit this file
# Generated by setup-matrix-rooms.sh
MATRIX_TESTUSER_PASSWORD=${password}
MATRIX_DAEMON_PASSWORD=${password}
PWEOF
    chmod 600 "$password_file"
  fi

  echo "$password"
}

# ---------------------------------------------------------------------------
# Register a Matrix user via client-server API
# Returns the access token on success, empty string if user already exists.
# ---------------------------------------------------------------------------
register_user() {
  local username="$1"
  local password="$2"
  local display_name="$3"

  info "Registering user @${username} ..."

  if $DRY_RUN; then
    info "[dry-run] Would register @${username}"
    echo "dry-run-token-${username}"
    return 0
  fi

  local response
  response=$(curl "${CURL_TLS_ARGS[@]}" -sf -X POST "$HOMESERVER/_matrix/client/v3/register" \
    --connect-timeout "$CURL_CONNECT_TIMEOUT_S" --max-time "$CURL_MAX_TIME_S" \
    -H "Content-Type: application/json" \
    -d "{
      \"username\": \"${username}\",
      \"password\": \"${password}\",
      \"auth\": {\"type\": \"m.login.dummy\"},
      \"inhibit_login\": false
    }" 2>&1) || true

  # Check if registration succeeded
  local token
  token=$(echo "$response" | python3 -c "import sys,json; print(json.load(sys.stdin).get('access_token',''))" 2>/dev/null || true)

  if [[ -n "$token" ]]; then
    info "Registered @${username} successfully"

    # Set display name
    local user_id="@${username}:${SERVER_NAME}"
    curl "${CURL_TLS_ARGS[@]}" -sf -X PUT "$HOMESERVER/_matrix/client/v3/profile/${user_id}/displayname" \
      -H "Authorization: Bearer ${token}" \
      -H "Content-Type: application/json" \
      -d "{\"displayname\": \"${display_name}\"}" >/dev/null 2>&1 || true

    echo "$token"
    return 0
  fi

  # User may already exist -- try logging in
  warn "Registration failed for @${username} (may already exist). Attempting login..."
  response=$(curl "${CURL_TLS_ARGS[@]}" -sf -X POST "$HOMESERVER/_matrix/client/v3/login" \
    --connect-timeout "$CURL_CONNECT_TIMEOUT_S" --max-time "$CURL_MAX_TIME_S" \
    -H "Content-Type: application/json" \
    -d "{
      \"type\": \"m.login.password\",
      \"identifier\": {\"type\": \"m.id.user\", \"user\": \"${username}\"},
      \"password\": \"${password}\"
    }" 2>&1) || true

  token=$(echo "$response" | python3 -c "import sys,json; print(json.load(sys.stdin).get('access_token',''))" 2>/dev/null || true)

  if [[ -n "$token" ]]; then
    info "Logged in as @${username}"
    echo "$token"
    return 0
  fi

  error "Failed to register or login @${username}"
  return 1
}

# ---------------------------------------------------------------------------
# Create a room with an alias, return room ID
# ---------------------------------------------------------------------------
create_room() {
  local alias="$1"
  local token="$2"
  local full_alias="#${alias}:${SERVER_NAME}"

  info "Creating room ${full_alias} ..."

  if $DRY_RUN; then
    info "[dry-run] Would create room ${full_alias}"
    echo "!dry-run-${alias}:localhost"
    return 0
  fi

  # Check if room already exists via alias lookup
  local existing
  existing=$(curl "${CURL_TLS_ARGS[@]}" -sf --connect-timeout "$CURL_CONNECT_TIMEOUT_S" --max-time "$CURL_MAX_TIME_S" \
    "$HOMESERVER/_matrix/client/v3/directory/room/%23${alias}:${SERVER_NAME}" 2>/dev/null || true)
  local existing_id
  existing_id=$(echo "$existing" | python3 -c "import sys,json; print(json.load(sys.stdin).get('room_id',''))" 2>/dev/null || true)

  if [[ -n "$existing_id" ]]; then
    info "Room ${full_alias} already exists: ${existing_id}"
    echo "$existing_id"
    return 0
  fi

  # Create the room
  local response
  response=$(curl "${CURL_TLS_ARGS[@]}" -sf -X POST "$HOMESERVER/_matrix/client/v3/createRoom" \
    --connect-timeout "$CURL_CONNECT_TIMEOUT_S" --max-time "$CURL_MAX_TIME_S" \
    -H "Authorization: Bearer ${token}" \
    -H "Content-Type: application/json" \
    -d "{
      \"room_alias_name\": \"${alias}\",
      \"name\": \"${alias}\",
      \"topic\": \"Symbiotic ${alias} channel\",
      \"visibility\": \"private\",
      \"preset\": \"private_chat\"
    }" 2>&1) || true

  local room_id
  room_id=$(echo "$response" | python3 -c "import sys,json; print(json.load(sys.stdin).get('room_id',''))" 2>/dev/null || true)

  if [[ -n "$room_id" ]]; then
    info "Created room ${full_alias}: ${room_id}"
    echo "$room_id"
    return 0
  fi

  error "Failed to create room ${full_alias}: ${response}"
  return 1
}

# ---------------------------------------------------------------------------
# Write room IDs to .env.rooms
# ---------------------------------------------------------------------------
write_rooms_env() {
  local -n room_map=$1

  info "Writing room IDs to ${ROOMS_ENV_FILE} ..."

  if $DRY_RUN; then
    info "[dry-run] Would write room IDs to ${ROOMS_ENV_FILE}"
    return 0
  fi

  mkdir -p "$CONFIG_DIR"
  {
    echo "# Symbiotic Matrix room IDs"
    echo "# Generated by setup-matrix-rooms.sh -- do not edit manually"
    echo "# $(date -u '+%Y-%m-%dT%H:%M:%SZ')"
    for alias in "${ROOM_ALIASES[@]}"; do
      local var_name
      var_name="SYMBIOTIC_MATRIX_ROOM_$(echo "$alias" | tr '[:lower:]' '[:upper:]')"
      echo "${var_name}=${room_map[$alias]}"
    done
  } > "$ROOMS_ENV_FILE"

  info "Room IDs written to ${ROOMS_ENV_FILE}"
}

# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------
main() {
  parse_args "$@"

  info "=========================================="
  info "Symbiotic Matrix Room Setup"
  info "=========================================="
  info "Homeserver:   $HOMESERVER"
  info "Server name:  $SERVER_NAME"
  info "Dry-run:      $DRY_RUN"
  info "=========================================="

  # Wait for homeserver
  wait_for_homeserver

  # Resolve runtime app user + password.
  load_runtime_user
  local app_password
  app_password=$(load_app_password)

  # Register/login app user.
  APP_TOKEN=$(register_user "$APP_USER" "$app_password" "$APP_DISPLAY")

  if [[ -z "$APP_TOKEN" ]]; then
    error "Failed to obtain token for @${APP_USER}"
    return 1
  fi

  # Create rooms and collect IDs
  declare -A room_ids
  for alias in "${ROOM_ALIASES[@]}"; do
    room_ids[$alias]=$(create_room "$alias" "$APP_TOKEN")
  done

  # Write .env.rooms
  write_rooms_env room_ids

  info "=========================================="
  info "Room setup complete"
  info "=========================================="
  info ""
  info "User ensured:"
  info "  @${APP_USER}:${SERVER_NAME}"
  info ""
  info "Rooms created:"
  for alias in "${ROOM_ALIASES[@]}"; do
    info "  #${alias}:${SERVER_NAME} -> ${room_ids[$alias]}"
  done
}

main "$@"
