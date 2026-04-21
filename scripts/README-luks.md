# LUKS Encryption Scripts

Scripts for managing LUKS2 full-disk encryption on the Symbiotic VPS data partition.

## Overview

These scripts automate LUKS2/dm-crypt setup for the Symbiotic data partition. LUKS encrypts the entire partition at rest, protecting all Tier 1 and Tier 2 data (Markdown files, SQLite databases, config files, logs) with zero application-level changes.

For the full data protection design, see `docs/design/tiered-data-protection.md`.

## Prerequisites

- **Root access** on a Debian/Ubuntu VPS
- **An empty partition** (e.g., `/dev/sdb`) — all data will be destroyed during provisioning
- **cryptsetup** installed: `apt install cryptsetup`
- **clevis** (optional, for Tang auto-unlock): `apt install clevis clevis-luks clevis-systemd`

## Scripts

| Script | Purpose |
|--------|---------|
| `luks-provision.sh` | One-time LUKS setup with verification gates |
| `luks-verify.sh` | Check LUKS status and health |
| `luks-unlock.sh` | Unlock and mount after reboot |
| `setup-luks.sh` | Legacy basic setup (passphrase only) |
| `unlock-luks.sh` | Legacy basic unlock (passphrase only) |

## Usage

### Initial Provisioning

**Passphrase method** (manual unlock on reboot):

```bash
sudo ./luks-provision.sh --device /dev/sdb --method passphrase
```

**Key file method** (generates a random key at `/root/.symbiotic-luks.key`):

```bash
sudo ./luks-provision.sh --device /dev/sdb --method keyfile
```

**Tang/Clevis method** (network-bound auto-unlock):

```bash
sudo ./luks-provision.sh --device /dev/sdb --method tang --tang-url http://tang.example.com
```

### Unlocking After Reboot

**With passphrase:**

```bash
sudo ./luks-unlock.sh --device /dev/sdb
```

**With key file:**

```bash
sudo ./luks-unlock.sh --device /dev/sdb --keyfile /root/.symbiotic-luks.key
```

**With Tang:** The partition auto-unlocks via `clevis-luks-askpass.service`. If the Tang server is unreachable, fall back to passphrase unlock:

```bash
sudo ./luks-unlock.sh --device /dev/sdb
```

### Checking Status

```bash
sudo ./luks-verify.sh --device /dev/sdb
```

Example output:

```
=== Symbiotic LUKS Status ===

  Device:     /dev/sdb
  ✓ Device exists
  ✓ LUKS2 configured
  ✓ /dev/mapper/symbiotic-data active
  ✓ Mounted at /var/lib/symbiotic
  ✓ Directories: data data/blob-store data/audit config
  ✓ Permissions: 0700 (mount point)
  ✓ Space: 47G free / 50G total (6% used)

  All checks passed
```

## Directory Structure

After provisioning, the mount point contains:

```
/var/lib/symbiotic/          # 0700
├── data/                    # 0700 — Archive store (.md files, SQLite indexes)
│   ├── blob-store/          # 0700 — Age-encrypted Tier 3 blobs (.age files)
│   └── audit/               # 0700 — Trust and access audit logs
└── config/                  # 0700 — Daemon configuration
```

## Systemd Integration

### Auto-unlock with passphrase (systemd-cryptsetup)

Uncomment the crypttab entry added by `luks-provision.sh`:

```bash
# /etc/crypttab
symbiotic-data  UUID=<your-uuid>  none  luks
```

This prompts for the passphrase during boot via `systemd-cryptsetup`.

### Auto-unlock with key file

```bash
# /etc/crypttab
symbiotic-data  UUID=<your-uuid>  /root/.symbiotic-luks.key  luks
```

### Auto-unlock with Tang

Enable the Clevis systemd unit:

```bash
systemctl enable clevis-luks-askpass.path
```

The partition will auto-unlock when the Tang server is reachable during boot.

### Systemd service dependency

To ensure the daemon starts only after the partition is mounted, add to the daemon's systemd unit:

```ini
[Unit]
RequiresMountsFor=/var/lib/symbiotic
After=local-fs.target
```

## Troubleshooting

### "Device is not a block device"

Verify the device path: `lsblk` or `fdisk -l`. Common paths: `/dev/sdb`, `/dev/vdb`, `/dev/nvme1n1p1`.

### "Device is already LUKS-formatted"

The partition was already provisioned. Use `luks-unlock.sh` to open it, or `luks-verify.sh` to check its status.

### Passphrase rejected during unlock

Ensure you are entering the passphrase set during `luks-provision.sh`. If using a key file, pass `--keyfile /root/.symbiotic-luks.key`.

### Missing directories after unlock

`luks-unlock.sh` automatically creates any missing directories from the expected structure. If directories are still missing, run the unlock script again.

### Tang server unreachable

Fall back to passphrase unlock: `sudo ./luks-unlock.sh --device /dev/sdb`. Check Tang server status: `curl -f http://tang.example.com/adv`.

### Disk space warnings

`luks-verify.sh` warns when usage exceeds 90%. Expand the partition or clean old audit logs:

```bash
find /var/lib/symbiotic/data/audit/ -mtime +90 -delete
```

### Key file lost

If the key file at `/root/.symbiotic-luks.key` is lost but you know the passphrase, add a new key slot:

```bash
sudo cryptsetup luksAddKey /dev/sdb /root/.symbiotic-luks.key.new
```
