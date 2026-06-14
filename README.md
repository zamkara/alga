# Alga

Ark Linux's graphical installer and system updater. Built with Rust, GTK4, and Libadwaita.

## Features

- Install Ark Linux to disk (UEFI only — systemd-boot or GRUB2)
- System update via `bootc upgrade` with live progress
- App self-update from the About page
- Configurable periodic update checks (app + OS) with native GNOME notifications
- BLS boot entry sync after every upgrade — always shows current + rollback deployment in boot menu
- Network connectivity check on launch

## Disk Layout

The installer partitions the target disk as follows:

| Partition | Size | Filesystem | Purpose |
|-----------|------|------------|---------|
| 1 | 1024 MiB | FAT32 (EFI-SYSTEM) | EFI System Partition |
| 2 | Remaining | btrfs (root) | OS root |

### Btrfs Subvolumes

| Subvolume | Mount Point |
|-----------|------------|
| `@` | `/` |
| `@var` | `/var` |
| `@var-log` | `/var/log` |
| `@var-cache` | `/var/cache` |
| `@var-tmp` | `/var/tmp` |
| `@tmp` | `/tmp` |
| `@snapshots` | `/.snapshots` |
| `@opt` | `/opt` |

All btrfs mounts use `compress=zstd,noatime` options. The `@snapshots` subvolume is compatible with Timeshift.

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/zamkara/alga/main/install.sh | sh
```

Installs alga and all required dependencies automatically. Supports Arch, Ubuntu/Debian, Fedora, and openSUSE.

### Dependencies

| Package | Purpose |
|---------|---------|
| `gtk4` | UI toolkit |
| `libadwaita` | GNOME UI components |
| `polkit` | Privilege escalation (`pkexec`) |
| `bootc` | OS install and upgrade |
| `ostree` | Deployment management |
| `skopeo` | Container image operations |
| `btrfs-progs` | Btrfs filesystem support |
| `dosfstools` | FAT/ESP formatting |
| `efibootmgr` | EFI boot entry management |
| `util-linux` | `blkid`, `lsblk`, `sfdisk` |

## Versioning

Releases use the GitHub Actions run number as version (e.g. `v70`). The version is baked in at build time via the `ALGA_BUILD_NUMBER` env var.

## Build

```sh
cargo build --release
```

For a release build matching CI:

```sh
ALGA_BUILD_NUMBER=<run_number> cargo build --release
```
