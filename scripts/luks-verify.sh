#!/usr/bin/env bash
# luks-verify.sh — Verify LUKS encryption status for Symbiotic data partition
#
# Usage:
#   sudo ./luks-verify.sh [OPTIONS]
#
# Options:
#   --device DEVICE     Block device to check (default: /dev/sdb)
#   --mount PATH        Expected mount point (default: /var/lib/symbiotic)
#   --quiet             Only print failures and exit code
#   --help              Show this help
#
# Exit codes:
#   0  All checks pass
#   1  One or more checks failed
#
# See also:
#   - luks-provision.sh — initial LUKS setup
#   - luks-unlock.sh    — unlock after reboot
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
YELLOW='\033[0;33m'
BOLD='\033[1m'
NC='\033[0m'

# ---------------------------------------------------------------------------
# State tracking
# ---------------------------------------------------------------------------
FAILURES=0
QUIET=false

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------
check_pass() {
    if [[ "$QUIET" != true ]]; then
        echo -e "  ${GREEN}✓${NC} $*"
    fi
}

check_fail() {
    echo -e "  ${RED}✗${NC} $*" >&2
    FAILURES=$((FAILURES + 1))
}

check_warn() {
    if [[ "$QUIET" != true ]]; then
        echo -e "  ${YELLOW}~${NC} $*"
    fi
}

usage() {
    sed -n '2,/^$/s/^# //p' "$0"
    exit 0
}

# ---------------------------------------------------------------------------
# Parse arguments
# ---------------------------------------------------------------------------
DEVICE="$DEFAULT_DEVICE"
MOUNT_POINT="$DEFAULT_MOUNT"

while [[ $# -gt 0 ]]; do
    case "$1" in
        --device) DEVICE="$2"; shift 2 ;;
        --mount)  MOUNT_POINT="$2"; shift 2 ;;
        --quiet)  QUIET=true; shift ;;
        --help|-h) usage ;;
        *)        echo "Unknown option: $1" >&2; exit 1 ;;
    esac
done

# ---------------------------------------------------------------------------
# Must be root for cryptsetup queries
# ---------------------------------------------------------------------------
if [[ $EUID -ne 0 ]]; then
    echo "This script must be run as root (use sudo)" >&2
    exit 1
fi

# ---------------------------------------------------------------------------
# Header
# ---------------------------------------------------------------------------
if [[ "$QUIET" != true ]]; then
    echo ""
    echo -e "${BOLD}=== Symbiotic LUKS Status ===${NC}"
    echo ""
    echo "  Device:     $DEVICE"
fi

# ---------------------------------------------------------------------------
# Check 1: Device exists
# ---------------------------------------------------------------------------
if [[ -b "$DEVICE" ]]; then
    check_pass "Device exists"
else
    check_fail "Device '$DEVICE' not found"
    # Can't continue without the device
    if [[ "$QUIET" != true ]]; then
        echo ""
        echo "  Result: $FAILURES check(s) failed"
    fi
    exit 1
fi

# ---------------------------------------------------------------------------
# Check 2: LUKS configured
# ---------------------------------------------------------------------------
if cryptsetup isLuks "$DEVICE" 2>/dev/null; then
    LUKS_VERSION=$(cryptsetup luksDump "$DEVICE" 2>/dev/null | grep "^Version:" | awk '{print $2}')
    if [[ "$LUKS_VERSION" == "2" ]]; then
        check_pass "LUKS2 configured"
    else
        check_warn "LUKS version $LUKS_VERSION (expected 2)"
    fi
else
    check_fail "Device is not LUKS-formatted"
fi

# ---------------------------------------------------------------------------
# Check 3: Mapper device active
# ---------------------------------------------------------------------------
if [[ -e "/dev/mapper/$MAPPER_NAME" ]]; then
    check_pass "/dev/mapper/$MAPPER_NAME active"
else
    check_fail "/dev/mapper/$MAPPER_NAME not found (partition is locked)"
fi

# ---------------------------------------------------------------------------
# Check 4: Mounted
# ---------------------------------------------------------------------------
if mountpoint -q "$MOUNT_POINT" 2>/dev/null; then
    check_pass "Mounted at $MOUNT_POINT"
else
    check_fail "Not mounted at $MOUNT_POINT"
fi

# ---------------------------------------------------------------------------
# Check 5: Directory structure
# ---------------------------------------------------------------------------
ALL_DIRS_OK=true
MISSING_DIRS=()
for subdir in "${EXPECTED_DIRS[@]}"; do
    if [[ ! -d "$MOUNT_POINT/$subdir" ]]; then
        ALL_DIRS_OK=false
        MISSING_DIRS+=("$subdir")
    fi
done

if [[ "$ALL_DIRS_OK" == true ]]; then
    check_pass "Directories: ${EXPECTED_DIRS[*]}"
else
    check_fail "Missing directories: ${MISSING_DIRS[*]}"
fi

# ---------------------------------------------------------------------------
# Check 6: Permissions
# ---------------------------------------------------------------------------
if [[ -d "$MOUNT_POINT" ]]; then
    PERMS=$(stat -c '%a' "$MOUNT_POINT" 2>/dev/null || stat -f '%Lp' "$MOUNT_POINT" 2>/dev/null || echo "unknown")
    if [[ "$PERMS" == "700" ]]; then
        check_pass "Permissions: 0700 (mount point)"
    else
        check_fail "Permissions: 0$PERMS (expected 0700)"
    fi
fi

# ---------------------------------------------------------------------------
# Check 7: Disk space
# ---------------------------------------------------------------------------
if mountpoint -q "$MOUNT_POINT" 2>/dev/null; then
    SPACE_INFO=$(df -h "$MOUNT_POINT" | tail -1)
    USED=$(echo "$SPACE_INFO" | awk '{print $3}')
    AVAIL=$(echo "$SPACE_INFO" | awk '{print $4}')
    TOTAL=$(echo "$SPACE_INFO" | awk '{print $2}')
    USE_PCT=$(echo "$SPACE_INFO" | awk '{print $5}')

    if [[ "$QUIET" != true ]]; then
        check_pass "Space: ${AVAIL} free / ${TOTAL} total (${USE_PCT} used)"
    fi

    # Warn if over 90% usage
    USE_NUM=${USE_PCT%\%}
    if [[ "$USE_NUM" -ge 90 ]]; then
        check_warn "Disk usage is above 90% — consider expanding"
    fi
fi

# ---------------------------------------------------------------------------
# Check 8: LUKS key slots
# ---------------------------------------------------------------------------
if cryptsetup isLuks "$DEVICE" 2>/dev/null; then
    SLOT_COUNT=$(cryptsetup luksDump "$DEVICE" 2>/dev/null | grep -c "^  [0-9]*: luks2" || true)
    if [[ "$QUIET" != true && "$SLOT_COUNT" -gt 0 ]]; then
        check_pass "Key slots: $SLOT_COUNT active"
    fi
fi

# ---------------------------------------------------------------------------
# Check 9: Clevis/Tang binding (if present)
# ---------------------------------------------------------------------------
if command -v clevis &>/dev/null; then
    TANG_BOUND=$(clevis luks list -d "$DEVICE" 2>/dev/null | grep -c "tang" || true)
    if [[ "$TANG_BOUND" -gt 0 ]]; then
        check_pass "Tang network-bound unlock configured"
    elif [[ "$QUIET" != true ]]; then
        check_warn "No Tang binding (manual unlock required after reboot)"
    fi
fi

# ---------------------------------------------------------------------------
# Result
# ---------------------------------------------------------------------------
if [[ "$QUIET" != true ]]; then
    echo ""
    if [[ $FAILURES -eq 0 ]]; then
        echo -e "  ${GREEN}${BOLD}All checks passed${NC}"
    else
        echo -e "  ${RED}${BOLD}$FAILURES check(s) failed${NC}"
    fi
    echo ""
fi

exit $((FAILURES > 0 ? 1 : 0))
