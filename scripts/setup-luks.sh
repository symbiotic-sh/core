#!/usr/bin/env bash
# setup-luks.sh - Set up LUKS2 full-disk encryption for Symbiotic VPS data partition.
#
# Usage:
#   sudo ./setup-luks.sh /dev/sdb
#
# This script:
#   1. Checks if the device is already a LUKS partition (idempotent)
#   2. Formats the device with LUKS2 encryption
#   3. Opens the LUKS device as 'symbiotic-data'
#   4. Creates an ext4 filesystem on the encrypted device
#   5. Mounts it to /var/lib/symbiotic
#   6. Creates required subdirectories: data/, config/, knowledge-base/ (Archive)
#
# Prerequisites:
#   - Must run as root (uses cryptsetup, mount, etc.)
#   - cryptsetup must be installed (apt install cryptsetup)
#   - The target device must exist and not be mounted
#
# Idempotency:
#   - If the device is already LUKS-formatted, it will not reformat
#   - If already mounted at /var/lib/symbiotic, it will not remount
#   - Subdirectories are created only if missing
#
# Security:
#   - LUKS2 with default cipher (aes-xts-plain64, 256-bit key)
#   - Protects all data at rest: .md files, SQLite, configs, logs
#   - Does NOT protect against runtime compromise (root attacker on running system)
#
# See also:
#   - unlock-luks.sh   — unlock and mount after reboot
#   - docs/design/tiered-data-protection.md — full design doc

set -euo pipefail

# ---------------------------------------------------------------------------
# Constants
# ---------------------------------------------------------------------------
MAPPER_NAME="symbiotic-data"
MOUNT_POINT="/var/lib/symbiotic"
SUBDIRS=("data" "config" "knowledge-base")

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------
log() { echo "[setup-luks] $*"; }
err() { echo "[setup-luks] ERROR: $*" >&2; exit 1; }

usage() {
    echo "Usage: sudo $0 DEVICE_PATH"
    echo ""
    echo "  DEVICE_PATH   Block device to encrypt (e.g., /dev/sdb, /dev/nvme1n1)"
    echo ""
    echo "Examples:"
    echo "  sudo $0 /dev/sdb"
    echo "  sudo $0 /dev/nvme1n1p1"
    exit 1
}

# ---------------------------------------------------------------------------
# Preflight checks
# ---------------------------------------------------------------------------
if [[ $# -lt 1 ]]; then
    usage
fi

DEVICE="$1"

if [[ $EUID -ne 0 ]]; then
    err "This script must be run as root (use sudo)"
fi

if ! command -v cryptsetup &>/dev/null; then
    err "cryptsetup is not installed. Run: apt install cryptsetup"
fi

if [[ ! -b "$DEVICE" ]]; then
    err "'$DEVICE' is not a block device or does not exist"
fi

# Check if device is currently mounted
if mount | grep -q "^${DEVICE} "; then
    err "'$DEVICE' is currently mounted. Unmount it first."
fi

# ---------------------------------------------------------------------------
# Step 1: LUKS format (idempotent — skip if already LUKS)
# ---------------------------------------------------------------------------
if cryptsetup isLuks "$DEVICE" 2>/dev/null; then
    log "Device '$DEVICE' is already LUKS-formatted — skipping format"
else
    log "Formatting '$DEVICE' with LUKS2 encryption..."
    log "You will be prompted to set a passphrase."
    log "WARNING: This will destroy all data on '$DEVICE'!"
    cryptsetup luksFormat --type luks2 "$DEVICE"
    log "LUKS format complete"
fi

# ---------------------------------------------------------------------------
# Step 2: Open LUKS device (idempotent — skip if already open)
# ---------------------------------------------------------------------------
if [[ -e "/dev/mapper/$MAPPER_NAME" ]]; then
    log "LUKS device already open as '/dev/mapper/$MAPPER_NAME' — skipping open"
else
    log "Opening LUKS device as '$MAPPER_NAME'..."
    log "Enter the passphrase you set during format."
    cryptsetup luksOpen "$DEVICE" "$MAPPER_NAME"
    log "LUKS device opened"
fi

# ---------------------------------------------------------------------------
# Step 3: Create filesystem (idempotent — skip if already ext4)
# ---------------------------------------------------------------------------
FS_TYPE=$(blkid -s TYPE -o value "/dev/mapper/$MAPPER_NAME" 2>/dev/null || true)

if [[ "$FS_TYPE" == "ext4" ]]; then
    log "Filesystem already exists on '/dev/mapper/$MAPPER_NAME' — skipping mkfs"
else
    log "Creating ext4 filesystem on '/dev/mapper/$MAPPER_NAME'..."
    mkfs.ext4 -L symbiotic "/dev/mapper/$MAPPER_NAME"
    log "Filesystem created"
fi

# ---------------------------------------------------------------------------
# Step 4: Mount (idempotent — skip if already mounted)
# ---------------------------------------------------------------------------
if mount | grep -q "/dev/mapper/$MAPPER_NAME on $MOUNT_POINT"; then
    log "Already mounted at '$MOUNT_POINT' — skipping mount"
else
    log "Mounting to '$MOUNT_POINT'..."
    mkdir -p "$MOUNT_POINT"
    mount "/dev/mapper/$MAPPER_NAME" "$MOUNT_POINT"
    log "Mounted at '$MOUNT_POINT'"
fi

# ---------------------------------------------------------------------------
# Step 5: Create required subdirectories
# ---------------------------------------------------------------------------
for subdir in "${SUBDIRS[@]}"; do
    target="$MOUNT_POINT/$subdir"
    if [[ -d "$target" ]]; then
        log "Directory '$target' already exists — skipping"
    else
        log "Creating '$target'..."
        mkdir -p "$target"
    fi
done

# Set ownership to the daemon user if it exists
if id -u symbiotic &>/dev/null; then
    chown -R symbiotic:symbiotic "$MOUNT_POINT"
    log "Set ownership to symbiotic:symbiotic"
fi

# Restrict permissions: owner read/write/exec only
chmod 700 "$MOUNT_POINT"
for subdir in "${SUBDIRS[@]}"; do
    chmod 700 "$MOUNT_POINT/$subdir"
done
log "Set permissions to 700 (owner only)"

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------
log ""
log "=== LUKS Setup Complete ==="
log "  Device:     $DEVICE"
log "  Mapper:     /dev/mapper/$MAPPER_NAME"
log "  Mount:      $MOUNT_POINT"
log "  Subdirs:    ${SUBDIRS[*]}"
log ""
log "After reboot, run: sudo ./unlock-luks.sh $DEVICE"
log "To add to fstab (optional):"
log "  echo '/dev/mapper/$MAPPER_NAME  $MOUNT_POINT  ext4  defaults  0  2' >> /etc/fstab"
