# Backup-Tool

A GTK4 desktop application for Fedora Linux that backs up your home directory to
an external drive using **rsync atomic backups** and a **systemd user timer**.
Full and incremental backups are automated; unchanged files across backups are
hardlinked so they consume no extra disk space.

---

## Features

- **GUI install wizard** — choose a backup drive from a live dropdown, set the
  schedule, and review exactly what will be installed before confirming
- **Atomic backups** — copies stage under `.inprogress-*` and are renamed
  only on success; a partial backup never appears as complete
- **Hardlinked incrementals** — unchanged files share disk blocks between
  backups via `--link-dest`; only deltas use new space
- **Automatic drive handling** — if the drive is not mounted at backup time the
  app attempts to mount it by UUID via `udisksctl` (no root required). Btrfs
  volumes are mounted with `compress-force=zstd:3` so rsync writes pack on disk
  (see [Btrfs compression](#btrfs-compression)).
- **Unattended mounting** — the RPM ships a polkit rule and a `backup-tool`
  group; members may mount the backup drive from the systemd timer without a
  prompt (`sudo usermod -aG backup-tool <user>`).  The rule grants only
  `udisks2.filesystem-mount` (external/removable drives); devices udisks2
  classifies as system devices (`filesystem-mount-system`, e.g. internal disks)
  still require administrator authentication.  If your backup drive is such an
  internal disk, add your own local rule for that device in
  `/etc/polkit-1/rules.d/`.
- **Retention policy** — incrementals from the current cycle are removed when
  the next full backup succeeds; orphaned incrementals older than
  `retention_days` are pruned as a safety net if a full is delayed
- **Full backup limit** — keep the newest `keep_full_snapshots` full backups
  (wizard default 12; `0` = unlimited).  Legacy configs without this field keep
  all full backups until you set a limit in the Schedule tab.
- **Exclude patterns** — editable per-line list stored in the config file;
  defaults exclude caches, trash, disc images, and browser data
- **Systemd user timer** — fully user-level (no root, no cron); survives reboots
  and can be toggled from the GUI
- **CLI headless mode** — `backup-tool backup auto` is what systemd calls;
  can also be invoked manually for scripting
- **Desktop integration** — `.desktop` launcher and icon installed to standard
  XDG paths; appears in GNOME Activities / application launchers

---

## Requirements

| Dependency | Purpose | Install |
|---|---|---|
| `rsync` | File transfer and backup copies | `sudo dnf install rsync` |
| `gtk4-devel` | Build-time GUI toolkit headers | `sudo dnf install gtk4-devel` |
| `cargo` / Rust ≥ 1.70 | Compile the binary | <https://rustup.rs> |
| `systemd` (user scope) | Scheduled backups | Already present on Fedora |
| `lsblk` / `util-linux` | Drive detection | Already present on Fedora |
| `udisksctl` *(optional)* | Auto-mount by UUID | `sudo dnf install udisks2` |

---

## Installation

> **Multi-user design:** the binary installs to `/usr/bin/backup-tool` so it
> is available to every user on the machine.  The systemd timer and config
> are per-user and are created by the first-run wizard.

### Option A — Install from GitHub release (simplest)

Download and install the pre-built RPM directly — no build tools needed:

```bash
sudo dnf install https://github.com/NormG/Backup-Tool/releases/download/v0.1.13/backup-tool-0.1.13-1.fc44.x86_64.rpm
backup-tool
```

Releases are signed with the Backup-Tool RPM key (fingerprint
`47DD 4557 7958 4B0E 2EFC  482E 2973 1FD9 8E42 2D05`). Import it once, then
check a downloaded RPM:

```bash
sudo rpmkeys --import https://raw.githubusercontent.com/NormG/Backup-Tool/master/packaging/backup-tool.gpg.asc
rpm -K backup-tool-*.rpm
```

Pass is `digests signatures OK`.

### Option B — Build RPM from source

Required when the pre-built RPM is not available for your Fedora release,
or when you want to compile from the latest source.

```bash
git clone https://github.com/NormG/Backup-Tool
cd Backup-Tool
rm -rf vendor && ./package-rpm.sh        # clean build — ~3 min
sudo dnf install ~/rpmbuild/RPMS/x86_64/backup-tool-0.1.13-1.fc44.x86_64.rpm
backup-tool
```

> **Always use `rm -rf vendor` before building a release RPM.**
> The `--no-vendor` flag reuses a cached Cargo build tree and can produce
> a binary compiled from stale source.  A clean build guarantees the RPM
> contains exactly what is in the repository.

Build requirements: `cargo`, `gtk4-devel`, `pkgconf-pkg-config`,
`rpm-build`, `rpmdevtools`. Signing a release also needs `rpm-sign`.

### Uninstall

```bash
sudo dnf remove backup-tool
```

See the full [Uninstall](#uninstall) section for notes on the systemd timer.

### Options for `install.sh` (alternative, no RPM)

```
./install.sh [options] [command]

Commands:
  install     Build and install to /usr/local/bin (default, requires sudo)
  uninstall   Remove all files; disable timer (prompts for confirmation)
  status      Show what is installed and whether the timer is active

Options:
  --user        Install to ~/.local/bin for the current user only
  --system      Install to /usr/local/bin (default, requires sudo)
  --skip-build  Use an existing target/release/backup-tool binary
  --yes         Skip the uninstall confirmation prompt
```

---

## First-run Setup Wizard

Launch the app for the first time (no config file exists yet) and the wizard
appears automatically:

| Page | What it does |
|---|---|
| **Welcome** | Explains the backup strategy |
| **Source** | Choose the directory to back up (default: your home folder) |
| **Drive** | Pick a partition from a live dropdown; press **Refresh** if the drive was just plugged in |
| **Schedule** | Set the full-backup day of week, daily time (24-hour), and retention |
| **Excludes** | Edit rsync exclude patterns one per line |
| **Review** | Shows every setting and the exact files that will be installed |
| **Done** | Recap of what was installed and when the first backup will run |

After the wizard completes, the systemd timer is enabled and the main manager
window opens.

---

## Main Window

The manager has seven tabs:

### Dashboard
- Current config summary with live drive-label detection
- Timer status and next scheduled run
- **Run now** buttons: *Auto (smart)* · *Force Full* · *Force Incremental*
- Toggle switch to enable / disable the scheduled timer

### Schedule
- Full-backup day of week
- Daily backup time with **12h / 24h toggle** — zero-padded spinbuttons, AM/PM selector
- Incremental backup period (1 = daily … 7 = weekly)
- Retention days for orphaned incrementals (safety net) and full backup count
- **Save & Reload Timer** rewrites the systemd timer unit and restarts it

### Excludes
- Edit the rsync exclude pattern list
- **Reset to defaults** restores the built-in pattern set
- **Save** writes to the config file; takes effect on the next backup

### Paths
- Change the source directory (Browse button)
- Change the backup destination path (Browse button)
- Drive UUID and label displayed for reference
- Same-filesystem guard prevents saving an unsafe configuration

### Log
- Shows the content of `~/.local/share/backup-tool/backup.log` (current cycle
  only; older cycles are archived inside each full backup)
- **Reload** refreshes the view

### About
- Application name, version, and description
- License, GitHub repository URL
- Backup engine, scheduler, config and log paths
- Copyright

### BTRFS
Only active when the source directory is on a BTRFS filesystem; otherwise
displays a notice and is otherwise inert.
- Snapshot storage path (default: `<dest_dir>/.btrfs-snapshots/`)
- **Create Snapshot** — creates a read-only `btrfs subvolume snapshot -r`
- List of existing snapshots, newest first; **Refresh** rescans
- **Recovery instructions** panel — populated when a snapshot is selected;
  shows exact `mount`, file-copy, and full-restore commands with the resolved
  device path filled in
- **Delete Selected Snapshot** — calls `btrfs subvolume delete`

Requires `btrfs-progs`: `sudo dnf install btrfs-progs`

---

## CLI Backup Mode

The same binary is called by systemd when the timer fires:

```bash
backup-tool backup auto         # full on configured day, incremental otherwise
backup-tool backup full         # force a full backup
backup-tool backup incremental  # force an incremental backup
```

Exit codes follow standard UNIX conventions (0 = success, non-zero = error).
rsync exit code 24 (files vanished during transfer) is treated as non-fatal.

---

## Configuration

Config file: `~/.config/backup-tool/config.toml`

```toml
source_dir       = "/home/norm"
dest_dir         = "/mnt/home_backups/myhostname"
drive_uuid       = "652e0201-35c2-4460-9709-2f620ddc4d22"
drive_label      = "Backup"
full_backup_day  = "Monday"
backup_time      = "02:00"
retention_days       = 30
keep_full_snapshots  = 12
installed        = true

excludes = [
    ".cache/",
    ".thumbnails/",
    ".var/app/*/cache/",
    ".mozilla/firefox/*/cache2/",
    ".config/google-chrome/*/Cache/",
    ".config/chromium/*/Cache/",
    ".local/share/Trash/",
    ".Trash-*/",
    "*.iso",
    ".extras/",
    "lost+found/",
    ".gvfs/",
    ".cargo/",     # Rust registry, git checkouts, compiled bins — all regenerable
    "*~",
]
```

All fields can be edited in the GUI. Saving from any tab writes to this file
immediately.

> **Tip:** If you want to keep `~/.cargo/credentials.toml` or `~/.cargo/bin/`
> backed up but skip the large cache, replace `.cargo/` with
> `.cargo/registry/` and `.cargo/git/` in the Excludes tab.

---

## Backup layout on drive

```
/mnt/home_backups/myhostname/
├── full-2026-07-21_020015/     ← weekly full (hardlinked to the previous snapshot, checksum-verified)
├── inc-2026-07-22_020008/      ← incremental (files unchanged vs full are hardlinked)
├── inc-2026-07-23_020011/
│   └── ...
├── inc-2026-07-26_020004/
└── latest -> inc-2026-07-26_020004   ← always points to the newest backup
```

Each full backup also stores the previous week's operational log at
`full-YYYY-MM-DD_HHmmss/.backup-tool/backup.log` (rotated automatically when
the full succeeds).  Search archived logs alongside the backup they describe.

Hardlinked files in incremental backups appear as complete directories but
share disk blocks with the full (or previous incremental) they were derived from.

**A few things worth knowing about disk usage and pruning:**

- **Incrementals reset on each full** — when a full backup succeeds, all
  `inc-*` backups from the previous cycle are deleted automatically.
  Directories that `rsync --archive` copied as read-only (Go module caches are
  0555) get owner rwx before delete; otherwise prune fails with permission
  denied and the incrementals stay.
- **Full backup limit** — only the newest `keep_full_snapshots` full backups
  are kept when set to a positive number (wizard default 12).  Set to `0` to
  keep all full backups.  Upgraded configs omit this field until you save a
  limit from the Schedule tab.
- **Orphan incrementals** — if a scheduled full is delayed, incrementals older
  than `retention_days` are removed as a safety net.
- **Weekly log rotation** — on each successful full backup, operational log
  lines (`[timestamp] …`) are archived into that full backup under
  `.backup-tool/backup.log` and the active log is truncated.  Per-file rsync
  detail goes to a temp file that is discarded.
- **Full backups are hardlinked and checksum-verified** — when a previous
  snapshot exists, a full backup runs `rsync --link-dest=<latest> --checksum`:
  every file is re-read and compared by content (so a silently corrupted copy on
  the backup drive is replaced), but unchanged data stays hardlinked and only
  changed data is written.  A full therefore needs about the same free space as
  an incremental (roughly 2% of the drive, with a floor of 10 GiB or 10% of the
  drive, whichever is smaller), not a second complete copy.  It does take longer
  because both sides are read in full.  Only the very first full backup (no
  earlier snapshot) is a plain complete copy.

---

## Btrfs compression

On a Btrfs backup drive, new backup data is compressed with zstd:

- Auto-mounts via `udisksctl` pass `compress-force=zstd:3`.
- Before each backup the tool also sets the directory property
  `btrfs property set <dest> compression zstd` (and on the volume root), so data
  is compressed even if the drive was mounted some other way.
- The tool never runs `chattr +c`: on Btrfs that flag sets the compression
  property to **zlib**, overriding zstd.
- Each backup logs whether the volume is mounted with `compress-force=zstd`
  and what the destination's compression property is.

Check it:

```bash
findmnt -no OPTIONS /run/media/$USER/Backup      # expect compress-force=zstd:3
btrfs property get <dest_dir> compression        # expect compression=zstd
```

### Recompress existing backups (manual, optional)

Only *new* writes are compressed.  Snapshots written before this was fixed stay
as they are until rewritten.  To recompress them (needs root, can take many
hours, and the drive needs some free space while it runs):

```bash
sudo compsize -x /run/media/$USER/Backup          # current ratio (dnf install compsize)
sudo btrfs filesystem defragment -r -v -czstd <dest_dir>
sudo compsize -x /run/media/$USER/Backup          # ratio afterwards
```

Hardlinks are unaffected (they are one inode); run it while no backup is
running.

---

## Systemd Units

Installed to `~/.config/systemd/user/`:

**`backup-tool.service`** — runs `backup-tool backup auto`; type `oneshot`
with a 1-hour stop timeout so large backups are not killed mid-run.

**`backup-tool.timer`** — fires daily at the configured time; `Persistent=true`
so a missed backup (e.g. machine was off) runs immediately on the next boot.
`RandomizedDelaySec=300` staggers the start by up to 5 minutes to avoid
constant contention if multiple machines back up to the same drive.

Useful commands:

```bash
# Status and scheduling
systemctl --user status backup-tool.timer
systemctl --user list-timers backup-tool.timer
journalctl --user -u backup-tool.service --since today

# Manually trigger a backup
systemctl --user start backup-tool.service

# Stop a running backup (e.g. to cancel a test run)
systemctl --user stop backup-tool.service
systemctl --user reset-failed backup-tool.service   # clear the failed state

# Disable / re-enable the scheduled timer
systemctl --user disable --now backup-tool.timer
systemctl --user enable  --now backup-tool.timer
```

> **Note:** If you stop a running backup manually, the `.inprogress-*`
> staging directory will be left on the drive.  Remove it before the
> next run to avoid confusion:
> ```bash
> rm -rf /path/to/dest/.inprogress-*
> ```

---

## Uninstall

### RPM install

```bash
sudo dnf remove backup-tool
```

This removes the system binary, launcher, and icons.  The user-level
systemd timer (`~/.config/systemd/user/backup-tool.timer`) is **not**
removed by `dnf` — it lives in your home directory and will continue to
fire.  Disable it manually after removing the RPM:

```bash
systemctl --user disable --now backup-tool.timer
```

### User install (`install.sh`)

```bash
./install.sh uninstall        # prompts for confirmation
./install.sh uninstall --yes  # no prompt
```

This disables the timer, removes the binary, launcher, icons, and assets.
**Your config file and backups on the drive are never deleted.**

To also remove the config:

```bash
rm -rf ~/.config/backup-tool
```

---

## Development

```bash
cargo build          # debug build
cargo check          # type-check without linking
cargo fmt            # format all source files
cargo build --release   # optimised stripped binary

# Run the GUI directly from the build directory:
./target/debug/backup-tool

# Test the backup engine without the GUI (needs a config file):
./target/debug/backup-tool backup full
```

### Project Layout

```
Backup-Tool/
├── src/
│   ├── main.rs          Entry point; CLI argument dispatch
│   ├── config.rs        Config struct, XDG load/save
│   ├── drives.rs        lsblk JSON parser, udisksctl mount helpers
│   ├── backup.rs        rsync execution, backup logic, retention
│   ├── systemd.rs       Unit file writer, enable/disable, desktop install
│   └── ui/
│       ├── mod.rs        GTK Application bootstrap
│       ├── install.rs    First-run setup wizard (7-page Stack)
│       └── main_win.rs   Management window (7-tab Notebook)
├── assets/
│   ├── backup-tool.png   128×128 app icon
│   ├── backup-tool.svg   Scalable app icon
│   └── backup-tool.desktop  Launcher template (@EXEC@ substituted on install)
├── backup-tool.spec     RPM spec file
├── package-rpm.sh       Build script for Fedora RPM packages
├── install.sh           Build / install / uninstall / status script
├── Cargo.toml
└── README.md
```

---

## License

GPL-3.0-or-later

## Fix for Mounting Issues

This tool has been modified to prevent mounting issues when running via systemd. The fix ensures that:

1. The backup tool checks if the destination path already exists
2. If it exists, the tool proceeds with the backup without attempting to mount
3. If it doesn't exist, the tool checks if the drive is already mounted by UUID
4. Only attempts to mount if the drive is not already mounted

This prevents the polkit authorization errors that occur when systemd user services try to mount drives without user interaction.
