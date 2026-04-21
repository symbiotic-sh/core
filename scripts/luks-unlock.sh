#!/usr/bin/env bash
# luks-unlock.sh — Unlock and mount the Symbiotic LUKS partition after reboot
#
# Usage:
#   sudo ./luks-unlock.sh [OPTIONS]
#
# Options:
#   --device DEVICE         Block device to unlock (default: /dev/sdb)
#   --mount PATH            Mount point (default: /var/lib/symbiotic)
#   --keyfile PATH          Key file for unlocking (instead of passphrase)
#   --help                  Show this help
#
# This is the enhanced unlock script that supports both passphrase and keyfile
# unlock methods. Run it after each VPS reboot to make the encrypted data
# partition available to the Symbiotic daemon.
#
# If the partition is already unlocked and mounted, this script does nothing
# (idempotent).
#
# For Tang/Clevis auto-unlock, the partition should unlock automatically via
# clevis-luks-askpass.service. Use this script as a fallback.
#
# See also:
#   - luks-provision.sh — initial LUKS setup
#   - luks-verify.sh    — check LUKS status
#   - README-luks.md    — full documentation

set -euo pipefail

# ---------------------------------------------------------------------------
# Constants
# ---------------------------------------------------------------------------
MAPPER_NAME="symbiotic-data"
DEFAULT_DEVICE="/dev/sdb"
DEFAULT_MOUNT="/var/lib/symbiotic"
EXPECTED_DIRS=("data" "data/blob-store" "data/audit" "config")

# ---------------------------------------------------------------------------
# Colors
# ---------------------------------------------------------------------------
RED='\033[0;31m'
GREEN='\033[0;32m'
BOLD='\033[1m'
NC='\033[0m'

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------
log()  { echo -e "${BOLD}[luks-unlock]${NC} $*"; }
ok()   { echo -e "${BOLD}[luks-unlock]${NC} ${GREEN}OK${NC}: $*"; }
err()  { echo -e "${BOLD}[luks-unlock]${NC} ${RED}ERROR${NC}: $*" >&2; exit 1; }

usage() {
    sed -n '2,/^$/s/^# //p' "$0"
    exit 0
}

# ---------------------------------------------------------------------------
# Parse arguments
# ---------------------------------------------------------------------------
DEVICE="$DEFAULT_DEVICE"
MOUNT_POINT="$DEFAULT_MOUNT"
KEYFILE=""

while [[ $# -gt 0 ]]; do
    case "$1" in
        --device)  DEVICE="$2"; shift 2 ;;
        --mount)   MOUNT_POINT="$2"; shift 2 ;;
        --keyfile) KEYFILE="$2"; shift 2 ;;
        --help|-h) usage ;;
        *)         err "Unknown option: $1. Use --help for usage." ;;
    esac
done

# ---------------------------------------------------------------------------
# Preflight checks
# ---------------------------------------------------------------------------
if [[ $EUID -ne 0 ]]; then
    err "This script must be run as root (use sudo)"
fi

if ! command -v cryptsetup &>/dev/null; then
    err "cryptsetup is not installed. Run: apt install cryptsetup"
fi

if [[ ! -b "$DEVICE" ]]; then
    err "'$DEVICE' is not a block device or does not exist"
fi

if ! cryptsetup isLuks "$DEVICE" 2>/dev/null; then
    err "'$DEVICE' is not LUKS-formatted. Run luks-provision.sh first."
fi

if [[ -n "$KEYFILE" && ! -f "$KEYFILE" ]]; then
    err "Key file '$KEYFILE' not found"
fi

# ---------------------------------------------------------------------------
# Step 1: Open LUKS device (idempotent)
# ---------------------------------------------------------------------------
if [[ -e "/dev/mapper/$MAPPER_NAME" ]]; then
    log "LUKS device already unlocked (/dev/mapper/$MAPPER_NAME)"
else
    log "Unlocking LUKS device '$DEVICE'..."
    if [[ -n "$KEYFILE" ]]; then
        cryptsetup luksOpen --key-file "$KEYFILE" "$DEVICE" "$MAPPER_NAME"
    else
        cryptsetup luksOpen "$DEVICE" "$MAPPER_NAME"
    fi
    ok "LUKS device unlocked"
fi

# Verify mapper exists
if [[ ! -e "/dev/mapper/$MAPPER_NAME" ]]; then
    err "/dev/mapper/$MAPPER_NAME not found after unlock attempt"
fi

# ---------------------------------------------------------------------------
# Step 2: Mount (idempotent)
# ---------------------------------------------------------------------------
if mountpoint -q "$MOUNT_POINT" 2>/dev/null; then
    log "Already mounted at '$MOUNT_POINT'"
else
    log "Mounting to '$MOUNT_POINT'..."
    mkdir -p "$MOUNT_POINT"
    mount "/dev/mapper/$MAPPER_NAME" "$MOUNT_POINT"
    ok "Mounted at '$MOUNT_POINT'"
fi

# ---------------------------------------------------------------------------
# Step 3: Verify directory structure
# ---------------------------------------------------------------------------
MISSING=()
for subdir in "${EXPECTED_DIRS[@]}"; do
    if [[ ! -d "$MOUNT_POINT/$subdir" ]]; then
        MISSING+=("$subdir")
    fi
done

if [[ ${#MISSING[@]} -gt 0 ]]; then
    log "Creating missing directories: ${MISSING[*]}"
    for subdir in "${MISSING[@]}"; do
        mkdir -p "$MOUNT_POINT/$subdir"
    done
fi

# ---------------------------------------------------------------------------
# Step 4: Set correct permissions
# ---------------------------------------------------------------------------
chmod 0700 "$MOUNT_POINT"
for subdir in "${EXPECTED_DIRS[@]}"; do
    if [[ -d "$MOUNT_POINT/$subdir" ]]; then
        chmod 0700 "$MOUNT_POINT/$subdir"
    fi
done

if id -u symbiotic &>/dev/null; then
    chown -R symbiotic:symbiotic "$MOUNT_POINT"
fi

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------
echo ""
echo -e "${BOLD}=== LUKS Unlock Complete ===${NC}"
echo ""
echo "  Device:  $DEVICE"
echo "  Mapper:  /dev/mapper/$MAPPER_NAME"
echo "  Mount:   $MOUNT_POINT"
echo ""
echo "  The Symbiotic daemon can now be started."
echo "  Run ./luks-verify.sh for a full status check."
