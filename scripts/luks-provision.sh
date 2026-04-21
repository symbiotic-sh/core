#!/usr/bin/env bash
# luks-provision.sh — LUKS2 Full-Disk Encryption Provisioning for Symbiotic VPS
#
# Usage:
#   sudo ./luks-provision.sh [OPTIONS]
#
# Options:
#   --device DEVICE     Block device to encrypt (default: /dev/sdb)
#   --mount PATH        Mount point (default: /var/lib/symbiotic)
#   --method METHOD     Unlock method: passphrase, keyfile, tang (default: passphrase)
#   --tang-url URL      Tang server URL (required when --method tang)
#   --yes               Skip confirmation prompt (for automation)
#   --help              Show this help
#
# This script sets up LUKS2 full-disk encryption on the Symbiotic data partition.
# It must be run as root during initial VPS provisioning. This is the comprehensive
# provisioning script with verification gates; for basic setup, see setup-luks.sh.
#
# Methods:
#   passphrase  — Prompt for a passphrase (manual unlock on reboot)
#   keyfile     — Generate a random key file stored at /root/.symbiotic-luks.key
#   tang        — Configure network-bound encryption via Tang/Clevis (auto-unlock)
#
# Verification gates run after each step to confirm success before proceeding.
#
# See also:
#   - luks-verify.sh    — check LUKS status at any time
#   - luks-unlock.sh    — unlock and mount after reboot
#   - unlock-luks.sh    — legacy unlock script (passphrase only)
#   - README-luks.md    — full documentation
#   - docs/design/tiered-data-protection.md — design doc

set -euo pipefail

# ---------------------------------------------------------------------------
# Constants
# ---------------------------------------------------------------------------
MAPPER_NAME="symbiotic-data"
DEFAULT_DEVICE="/dev/sdb"
DEFAULT_MOUNT="/var/lib/symbiotic"
DEFAULT_METHOD="passphrase"
KEYFILE_PATH="/root/.symbiotic-luks.key"
KEYFILE_SIZE=4096
SUBDIRS=("data" "data/blob-store" "data/audit" "config")

# ---------------------------------------------------------------------------
# Colors
# ---------------------------------------------------------------------------
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[0;33m'
BOLD='\033[1m'
NC='\033[0m' # No color

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------
log()  { echo -e "${BOLD}[luks-provision]${NC} $*"; }
ok()   { echo -e "${BOLD}[luks-provision]${NC} ${GREEN}OK${NC}: $*"; }
warn() { echo -e "${BOLD}[luks-provision]${NC} ${YELLOW}WARN${NC}: $*" >&2; }
err()  { echo -e "${BOLD}[luks-provision]${NC} ${RED}ERROR${NC}: $*" >&2; exit 1; }

gate_pass() { echo -e "  ${GREEN}PASS${NC} $*"; }
gate_fail() { echo -e "  ${RED}FAIL${NC} $*" >&2; exit 1; }

usage() {
    sed -n '2,/^$/s/^# //p' "$0"
    exit 0
}

# ---------------------------------------------------------------------------
# Parse arguments
# ---------------------------------------------------------------------------
DEVICE="$DEFAULT_DEVICE"
MOUNT_POINT="$DEFAULT_MOUNT"
METHOD="$DEFAULT_METHOD"
TANG_URL=""
SKIP_CONFIRM=false

while [[ $# -gt 0 ]]; do
    case "$1" in
        --device)   DEVICE="$2"; shift 2 ;;
        --mount)    MOUNT_POINT="$2"; shift 2 ;;
        --method)   METHOD="$2"; shift 2 ;;
        --tang-url) TANG_URL="$2"; shift 2 ;;
        --yes)      SKIP_CONFIRM=true; shift ;;
        --help|-h)  usage ;;
        *)          err "Unknown option: $1. Use --help for usage." ;;
    esac
done

# Validate method
case "$METHOD" in
    passphrase|keyfile|tang) ;;
    *) err "Invalid method '$METHOD'. Must be one of: passphrase, keyfile, tang" ;;
esac

if [[ "$METHOD" == "tang" && -z "$TANG_URL" ]]; then
    err "--tang-url is required when using --method tang"
fi

# ---------------------------------------------------------------------------
# Preflight checks
# ---------------------------------------------------------------------------
log "Starting LUKS2 provisioning"
log "  Device:  $DEVICE"
log "  Mount:   $MOUNT_POINT"
log "  Method:  $METHOD"
echo ""

if [[ $EUID -ne 0 ]]; then
    err "This script must be run as root (use sudo)"
fi

if ! command -v cryptsetup &>/dev/null; then
    err "cryptsetup is not installed. Run: apt install cryptsetup"
fi

if [[ ! -b "$DEVICE" ]]; then
    err "'$DEVICE' is not a block device or does not exist"
fi

if mount | grep -q "^${DEVICE} "; then
    err "'$DEVICE' is currently mounted. Unmount it first."
fi

if cryptsetup isLuks "$DEVICE" 2>/dev/null; then
    err "'$DEVICE' is already LUKS-formatted. Use luks-unlock.sh to open it, or luks-verify.sh to check status."
fi

if [[ "$METHOD" == "tang" ]]; then
    if ! command -v clevis &>/dev/null; then
        err "clevis is not installed. Run: apt install clevis clevis-luks clevis-systemd"
    fi
fi

# ---------------------------------------------------------------------------
# Confirmation gate
# ---------------------------------------------------------------------------
echo -e "${RED}${BOLD}WARNING: This will DESTROY all data on $DEVICE${NC}"
echo ""

if [[ "$SKIP_CONFIRM" != true ]]; then
    read -r -p "Type 'YES' to continue: " confirm
    if [[ "$confirm" != "YES" ]]; then
        log "Aborted by user."
        exit 0
    fi
    echo ""
fi

# ---------------------------------------------------------------------------
# Step 1: LUKS format
# ---------------------------------------------------------------------------
log "Step 1/6: Formatting '$DEVICE' with LUKS2..."

case "$METHOD" in
    passphrase)
        cryptsetup luksFormat --type luks2 "$DEVICE"
        ;;
    keyfile)
        log "Generating ${KEYFILE_SIZE}-byte random key file at $KEYFILE_PATH..."
        dd if=/dev/urandom of="$KEYFILE_PATH" bs=1 count="$KEYFILE_SIZE" 2>/dev/null
        chmod 0400 "$KEYFILE_PATH"
        cryptsetup luksFormat --type luks2 "$DEVICE" "$KEYFILE_PATH"
        ok "Key file written to $KEYFILE_PATH (permissions 0400)"
        ;;
    tang)
        # Format with passphrase first, then bind Tang
        cryptsetup luksFormat --type luks2 "$DEVICE"
        ;;
esac

# Verification gate: LUKS header
log "  Verifying LUKS2 header..."
if cryptsetup isLuks "$DEVICE" 2>/dev/null; then
    LUKS_VERSION=$(cryptsetup luksDump "$DEVICE" 2>/dev/null | grep "^Version:" | awk '{print $2}')
    if [[ "$LUKS_VERSION" == "2" ]]; then
        gate_pass "LUKS2 header verified on $DEVICE"
    else
        gate_fail "Expected LUKS version 2, got '$LUKS_VERSION'"
    fi
else
    gate_fail "Device is not LUKS-formatted after luksFormat"
fi

# ---------------------------------------------------------------------------
# Step 2: Open LUKS device
# ---------------------------------------------------------------------------
log "Step 2/6: Opening LUKS device as '$MAPPER_NAME'..."

case "$METHOD" in
    passphrase|tang)
        cryptsetup luksOpen "$DEVICE" "$MAPPER_NAME"
        ;;
    keyfile)
        cryptsetup luksOpen --key-file "$KEYFILE_PATH" "$DEVICE" "$MAPPER_NAME"
        ;;
esac

# Verification gate: mapper device
log "  Verifying mapper device..."
if [[ -e "/dev/mapper/$MAPPER_NAME" ]]; then
    gate_pass "/dev/mapper/$MAPPER_NAME exists"
else
    gate_fail "/dev/mapper/$MAPPER_NAME not found after luksOpen"
fi

# ---------------------------------------------------------------------------
# Step 3: Create filesystem
# ---------------------------------------------------------------------------
log "Step 3/6: Creating ext4 filesystem on '/dev/mapper/$MAPPER_NAME'..."
mkfs.ext4 -L symbiotic "/dev/mapper/$MAPPER_NAME"

# Verification gate: filesystem
log "  Verifying filesystem..."
FS_CHECK=$(tune2fs -l "/dev/mapper/$MAPPER_NAME" 2>/dev/null | grep "Filesystem magic number" || true)
if [[ -n "$FS_CHECK" ]]; then
    gate_pass "ext4 filesystem verified"
else
    gate_fail "ext4 filesystem verification failed"
fi

# ---------------------------------------------------------------------------
# Step 4: Mount
# ---------------------------------------------------------------------------
log "Step 4/6: Mounting to '$MOUNT_POINT'..."
mkdir -p "$MOUNT_POINT"
mount "/dev/mapper/$MAPPER_NAME" "$MOUNT_POINT"

# Verification gate: mount
log "  Verifying mount..."
if mountpoint -q "$MOUNT_POINT" 2>/dev/null; then
    gate_pass "$MOUNT_POINT is mounted"
else
    gate_fail "$MOUNT_POINT is not a mount point"
fi

# ---------------------------------------------------------------------------
# Step 5: Create directory structure and set permissions
# ---------------------------------------------------------------------------
log "Step 5/6: Creating directory structure..."

for subdir in "${SUBDIRS[@]}"; do
    target="$MOUNT_POINT/$subdir"
    mkdir -p "$target"
    log "  Created $target"
done

# Set permissions: 0700 on mount point and all subdirs
chmod 0700 "$MOUNT_POINT"
for subdir in "${SUBDIRS[@]}"; do
    chmod 0700 "$MOUNT_POINT/$subdir"
done

# Set ownership to daemon user if it exists
if id -u symbiotic &>/dev/null; then
    chown -R symbiotic:symbiotic "$MOUNT_POINT"
    ok "Ownership set to symbiotic:symbiotic"
fi

ok "Directory structure created with 0700 permissions"

# ---------------------------------------------------------------------------
# Step 6: Configure Tang/Clevis (if applicable)
# ---------------------------------------------------------------------------
if [[ "$METHOD" == "tang" ]]; then
    log "Step 6/6: Binding Tang/Clevis for network-bound unlock..."
    clevis luks bind -d "$DEVICE" tang "{\"url\":\"$TANG_URL\"}"
    ok "Tang binding complete — device will auto-unlock on trusted network"
else
    log "Step 6/6: Writing system config entries..."
fi

# ---------------------------------------------------------------------------
# Write /etc/crypttab entry (commented out for manual unlock by default)
# ---------------------------------------------------------------------------
CRYPTTAB_LINE="# $MAPPER_NAME  UUID=$(blkid -s UUID -o value "$DEVICE")  none  luks"
if [[ "$METHOD" == "keyfile" ]]; then
    CRYPTTAB_LINE="# $MAPPER_NAME  UUID=$(blkid -s UUID -o value "$DEVICE")  $KEYFILE_PATH  luks"
fi

if ! grep -q "$MAPPER_NAME" /etc/crypttab 2>/dev/null; then
    echo "$CRYPTTAB_LINE" >> /etc/crypttab
    ok "Added crypttab entry (commented out — uncomment for auto-unlock at boot)"
else
    warn "crypttab entry for '$MAPPER_NAME' already exists — skipping"
fi

# ---------------------------------------------------------------------------
# Write /etc/fstab entry
# ---------------------------------------------------------------------------
FSTAB_LINE="/dev/mapper/$MAPPER_NAME  $MOUNT_POINT  ext4  defaults  0  2"

if ! grep -q "$MAPPER_NAME" /etc/fstab 2>/dev/null; then
    echo "$FSTAB_LINE" >> /etc/fstab
    ok "Added fstab entry for $MOUNT_POINT"
else
    warn "fstab entry for '$MAPPER_NAME' already exists — skipping"
fi

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------
echo ""
echo -e "${BOLD}=== LUKS Provisioning Complete ===${NC}"
echo ""
echo "  Device:       $DEVICE"
echo "  LUKS version: 2"
echo "  Mapper:       /dev/mapper/$MAPPER_NAME"
echo "  Mounted at:   $MOUNT_POINT"
echo "  Method:       $METHOD"
echo "  Directories:  ${SUBDIRS[*]}"
echo ""

case "$METHOD" in
    passphrase)
        echo "  After reboot, run:"
        echo "    sudo ./luks-unlock.sh --device $DEVICE"
        ;;
    keyfile)
        echo "  Key file:     $KEYFILE_PATH"
        echo "  After reboot, run:"
        echo "    sudo ./luks-unlock.sh --device $DEVICE --keyfile $KEYFILE_PATH"
        echo ""
        echo -e "  ${YELLOW}IMPORTANT: Back up the key file to a secure location!${NC}"
        ;;
    tang)
        echo "  Tang server:  $TANG_URL"
        echo "  The partition will auto-unlock when the Tang server is reachable."
        echo "  Fallback: sudo ./luks-unlock.sh --device $DEVICE"
        ;;
esac

echo ""
echo "  Run ./luks-verify.sh to check status at any time."
