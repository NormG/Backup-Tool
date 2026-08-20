# Backup-Tool

A GTK4 desktop application for Fedora Linux that backs up your home directory to
an external drive using **rsync atomic snapshots** and a **systemd user timer**.
Full and incremental backups are automated; unchanged files across snapshots are
hardlinked so they consume no extra disk space.

---

## Features

- **GUI install wizard** — choose a backup drive from a live dropdown, set the
  schedule, and review exactly what will be installed before confirming
- **Atomic snapshots** — backups stage under `.inprogress-*` and are renamed
  only on success; a partial backup never appears as complete
- **Hardlinked incrementals** — unchanged files share disk blocks between
  snapshots via `--link-dest`; only deltas use new space
- **Automatic drive handling** — if the drive is not mounted at backup time the
  app attempts to mount it by UUID via `udisksctl` (no root required)
- **Retention policy** — incrementals from the current cycle are removed when
  the next full backup succeeds; orphaned incrementals older than
  `retention_days` are pruned as a safety net if a full is delayed
- **Full snapshot limit** — keep the newest `keep_full_snapshots` full backups
  (wizard default 12; `0` = unlimited).  Legacy configs without this field keep
  all full snapshots until you set a limit in the Schedule tab.
- **Exclude patterns** — editable per-line list stored in the config file;
  defaults exclude caches, trash, disc images, and browser data
- **Systemd user timer** — fully user-level (no root, no cron); survives reboots
  and can be toggled from the GUI
- **CLI headless mode** — `backup-tool --backup auto` is what systemd calls;
  can also be invoked manually for scripting
- **Desktop integration** — `.desktop` launcher and icon installed to standard
  XDG paths; appears in GNOME Activities / application launchers

---

## Requirements

| Dependency | Purpose | Install |
|---|---|---|
| `rsync` | File transfer and snapshotting | `sudo dnf install rsync` |
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
sudo dnf install https://github.com/NormG/Backup-Tool/releases/download/v0.1.8/backup-tool-0.1.8-1.fc44.x86_64.rpm
backup-tool
```

### Option B — Build RPM from source

Required when the pre-built RPM is not available for your Fedora release,
or when you want to compile from the latest source.

```bash
git clone https://github.com/NormG/Backup-Tool
cd Backup-Tool
rm -rf vendor && ./package-rpm.sh        # clean build — ~3 min
sudo dnf install ~/rpmbuild/RPMS/x86_64/backup-tool-0.1.8-1.fc44.x86_64.rpm
backup-tool
```

> **Always use `rm -rf vendor` before building a release RPM.**
> The `--no-vendor` flag reuses a cached Cargo build tree and can produce
> a binary compiled from stale source.  A clean build guarantees the RPM
> contains exactly what is in the repository.

Build requirements: `cargo`, `gtk4-devel`, `pkgconf-pkg-config`,
`rpm-build`, `rpmdevtools`.

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
- Retention days for orphaned incrementals (safety net) and full snapshot count
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
  only; older cycles are archived inside each full snapshot)
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
backup-tool --backup auto         # full on configured day, incremental otherwise
backup-tool --backup full         # force a full snapshot
backup-tool --backup incremental  # force an incremental snapshot
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

## Snapshot Layout

```
/mnt/home_backups/myhostname/
├── full-2026-07-21_020015/     ← weekly full copy
├── inc-2026-07-22_020008/      ← incremental (files unchanged vs full are hardlinked)
├── inc-2026-07-23_020011/
│   └── ...
├── inc-2026-07-26_020004/
└── latest -> inc-2026-07-26_020004   ← always points to the newest snapshot
```

Each full snapshot also stores the previous week's operational log at
`full-YYYY-MM-DD_HHmmss/.backup-tool/backup.log` (rotated automatically when
the full succeeds).  Search archived logs alongside the snapshot they describe.

Hardlinked files in incremental snapshots appear as complete directories but
share disk blocks with the full (or previous incremental) they were derived from.

**A few things worth knowing about disk usage and pruning:**

- **Incrementals reset on each full** — when a full backup succeeds, all
  `inc-*` snapshots from the previous cycle are deleted automatically.
- **Full snapshot limit** — only the newest `keep_full_snapshots` full backups
  are kept when set to a positive number (wizard default 12).  Set to `0` to
  keep all full snapshots.  Upgraded configs omit this field until you save a
  limit from the Schedule tab.
- **Orphan incrementals** — if a scheduled full is delayed, incrementals older
  than `retention_days` are removed as a safety net.
- **Weekly log rotation** — on each successful full backup, operational log
  lines (`[timestamp] …`) are archived into that full snapshot under
  `.backup-tool/backup.log` and the active log is truncated.  Per-file rsync
  detail goes to a temp file that is discarded.
- **Each full backup is a complete copy** — no hardlinks from previous fulls, so
  every full uses roughly the same space as your home directory.  Plan drive
  capacity for `keep_full_snapshots` full copies plus daily incrementals during
  each cycle.

---

## Systemd Units

Installed to `~/.config/systemd/user/`:

**`backup-tool.service`** — runs `backup-tool --backup auto`; type `oneshot`
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
**Your config file and backup snapshots are never deleted.**

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
./target/debug/backup-tool --backup full
```

### Project Layout

```
Backup-Tool/
├── src/
│   ├── main.rs          Entry point; CLI argument dispatch
│   ├── config.rs        Config struct, XDG load/save
│   ├── drives.rs        lsblk JSON parser, udisksctl mount helpers
│   ├── backup.rs        rsync execution, snapshot logic, retention
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
