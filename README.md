# Home Backup

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
- **Retention policy** — incremental snapshots older than a configurable number
  of days are pruned automatically
- **Exclude patterns** — editable per-line list stored in the config file;
  defaults exclude caches, trash, disc images, and browser data
- **Systemd user timer** — fully user-level (no root, no cron); survives reboots
  and can be toggled from the GUI
- **CLI headless mode** — `home-backup --backup auto` is what systemd calls;
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

```bash
git clone <repo-url> home-backup2
cd home-backup2
./install.sh
```

The script:
1. Checks all dependencies
2. Compiles a stripped release binary with `cargo build --release`
3. Installs the binary to `~/.local/bin/home-backup`
4. Installs the icon (128×128 PNG + SVG) to `~/.local/share/icons/hicolor/`
5. Installs `home-backup.desktop` to `~/.local/share/applications/`
6. Refreshes the icon and desktop databases

The **systemd timer is configured during the first-run wizard**, not by
`install.sh`, so no backup is scheduled until you run the app.

### Options

```
./install.sh [options] [command]

Commands:
  install     Build and install (default)
  uninstall   Remove all files; disable timer (prompts for confirmation)
  status      Show what is installed and whether the timer is active

Options:
  --system      Install to /usr/local/bin instead of ~/.local/bin (needs sudo)
  --skip-build  Use an existing target/release/home-backup binary; skip compile
  --yes         Skip the uninstall confirmation prompt
```

### PATH

If `~/.local/bin` is not in your `$PATH`, add this to `~/.bashrc`:

```bash
export PATH="${HOME}/.local/bin:${PATH}"
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

The manager has five tabs:

### Dashboard
- Current config summary with live drive-label detection
- Timer status and next scheduled run
- **Run now** buttons: *Auto (smart)* · *Force Full* · *Force Incremental*
- Toggle switch to enable / disable the scheduled timer

### Schedule
- Full-backup day of week
- Daily backup time with **12h / 24h toggle** — zero-padded spinbuttons, AM/PM selector
- Incremental backup period (1 = daily … 7 = weekly)
- Retention days for incremental snapshots
- **Save & Reload Timer** rewrites the systemd timer unit and restarts it

### Excludes
- Edit the rsync exclude pattern list
- **Reset to defaults** restores the built-in pattern set
- **Save** writes to the config file; takes effect on the next backup

### Source / Destination
- Change the source directory (Browse button)
- Change the backup destination path (Browse button)
- Drive UUID and label displayed for reference
- Same-filesystem guard prevents saving an unsafe configuration

### Log
- Shows the content of `~/.local/share/home-backup/backup.log`
- **Reload** refreshes the view

---

## CLI Backup Mode

The same binary is called by systemd when the timer fires:

```bash
home-backup --backup auto         # full on configured day, incremental otherwise
home-backup --backup full         # force a full snapshot
home-backup --backup incremental  # force an incremental snapshot
```

Exit codes follow standard UNIX conventions (0 = success, non-zero = error).
rsync exit code 24 (files vanished during transfer) is treated as non-fatal.

---

## Configuration

Config file: `~/.config/home-backup/config.toml`

```toml
source_dir       = "/home/norm"
dest_dir         = "/mnt/home_backups/myhostname"
drive_uuid       = "652e0201-35c2-4460-9709-2f620ddc4d22"
drive_label      = "Backup"
full_backup_day  = "Monday"
backup_time      = "02:00"
retention_days   = 30
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

Hardlinked files in incremental snapshots appear as complete directories but
share disk blocks with the full (or previous incremental) they were derived from.

**A few things worth knowing about disk usage and pruning:**

- **Full backups are never automatically pruned** — only incrementals older than
  `retention_days` are deleted.  Full snapshots accumulate until you remove them
  manually, so periodically review and delete old ones to free space.
- **Each full backup is a complete copy** — no hardlinks from previous fulls, so
  every weekly full uses roughly the same space as your home directory.  Plan
  drive capacity accordingly (e.g. 3 full backups + 6 incrementals each week).
- **Incrementals reset after each full** — on the day after a full backup,
  incrementals begin hardlinking from the new full, so they stay lean again.

---

## Systemd Units

Installed to `~/.config/systemd/user/`:

**`home-backup.service`** — runs `home-backup --backup auto`; type `oneshot`
with a 1-hour stop timeout so large backups are not killed mid-run.

**`home-backup.timer`** — fires daily at the configured time; `Persistent=true`
so a missed backup (e.g. machine was off) runs immediately on the next boot.

Check status at any time:

```bash
systemctl --user status home-backup.timer
systemctl --user list-timers home-backup.timer
journalctl --user -u home-backup.service --since today
```

---

## Uninstall

```bash
./install.sh uninstall        # prompts for confirmation
./install.sh uninstall --yes  # no prompt
```

This disables and removes the systemd timer and service, removes the binary,
icon, launcher, and assets directory. **Your config file and backup snapshots
are never deleted.**

To also remove the config:

```bash
rm -rf ~/.config/home-backup
```

---

## Development

```bash
cargo build          # debug build
cargo check          # type-check without linking
cargo fmt            # format all source files
cargo build --release   # optimised stripped binary

# Run the GUI directly from the build directory:
./target/debug/home-backup

# Test the backup engine without the GUI (needs a config file):
./target/debug/home-backup --backup full
```

### Project Layout

```
home-backup2/
├── src/
│   ├── main.rs          Entry point; CLI argument dispatch
│   ├── config.rs        Config struct, XDG load/save
│   ├── drives.rs        lsblk JSON parser, udisksctl mount helpers
│   ├── backup.rs        rsync execution, snapshot logic, retention
│   ├── systemd.rs       Unit file writer, enable/disable, desktop install
│   └── ui/
│       ├── mod.rs        GTK Application bootstrap
│       ├── install.rs    First-run setup wizard (7-page Stack)
│       └── main_win.rs   Management window (5-tab Notebook)
├── assets/
│   ├── home-backup.png   128×128 app icon
│   ├── home-backup.svg   Scalable app icon
│   └── home-backup.desktop  Launcher template (@EXEC@ substituted on install)
├── install.sh           Build / install / uninstall / status script
├── Cargo.toml
└── README.md
```

---

## License

GPL-3.0
