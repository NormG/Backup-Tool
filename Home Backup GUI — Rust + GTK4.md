# Home Backup — Rust + GTK4
## Overview
A GTK4 desktop application for Fedora Linux that backs up a home directory to an external drive using rsync atomic snapshots and a systemd user timer. The binary serves two modes: GUI mode (no arguments) for setup and management, and headless mode (`--backup auto|full|incremental`) called by systemd at the scheduled time.
## Architecture
A single stripped binary (~1.2 MB). Six Rust modules plus a two-file UI layer.
* `config.rs` — `Config` struct with XDG-aware TOML load/save; backward-compatible via `#[serde(default)]` on new fields.
* `drives.rs` — `lsblk` JSON parser; `is_same_device()` via `MetadataExt::dev()`; `detect_label_for_path()` via `findmnt` + `lsblk`; `mount_by_uuid()` via `udisksctl`.
* `backup.rs` — atomic rsync snapshots; `BackupKind::{Full, Incremental, Auto, Skip}`; `--link-dest` hardlinking; configurable incremental period; retention pruning; rsync exit-24 treated as non-fatal.
* `systemd.rs` — writes user service + timer units; Nautilus bookmark management (stale-entry cleanup before add); desktop launcher and icon install.
* `ui/install.rs` — seven-page `ApplicationWindow` wizard: Welcome, Source, Drive (live dropdown + Refresh), Schedule, Excludes, Review, Done/Recap.
* `ui/main_win.rs` — five-tab `ApplicationWindow` manager: Dashboard, Schedule, Excludes, Source/Destination, Log.
## Install wizard
Pages: Welcome → Source (Browse) → Drive (lsblk dropdown, auto-fills dest path from hostname) → Schedule → Excludes → Review (full recap of what will be written) → Done. The Review page shows all settings and the exact file paths that will be installed. Install is blocked if source and destination resolve to the same filesystem device. The wizard window is itself an `ApplicationWindow`; opening the manager before closing it prevents any zero-window app-exit gap.
## Management window
Five tabs.
* **Dashboard** — config summary with live drive-label detection, timer status and next-run time, Run Now buttons (Auto / Force Full / Force Incremental), timer enable/disable toggle.
* **Schedule** — full-backup day of week; daily time with 12h/24h toggle (zero-padded, right-aligned spinbuttons, AM/PM selector, toggle button right-justified); incremental period (1–7 days); retention days.
* **Excludes** — editable rsync exclude patterns; Reset to defaults; Save.
* **Source/Destination** — editable source and destination paths with Browse buttons; same-device guard on save.
* **Log** — scrollable rsync log viewer with Reload button.
## Backup behavior
Rsync flags: `--archive --delete --numeric-ids --human-readable --stats --exclude-from --log-file`. `--inplace` and `--partial` are intentionally omitted — they conflict with `--link-dest` and can corrupt snapshot hardlink integrity. Snapshots stage under `.inprogress-{type}-{stamp}` and are atomically renamed. The `latest` symlink always points to the newest snapshot. The `auto` mode selects Full on the configured day-of-week or when no full snapshot exists yet; selects Skip when the incremental period has not elapsed; otherwise selects Incremental.
## Safety
* Same-filesystem guard in both wizard and Source/Destination tab using `MetadataExt::dev()`, walking up to the nearest existing ancestor.
* `SYSTEM_MOUNTS` blocks `/`, `/home`, `/boot`, `/usr`, `/var`, `/tmp`, `/opt`, `/srv`, and swap from appearing in the drive dropdown.
* Uninstall refuses to run when `$PWD` is inside a snapshot directory or the configured backup destination.
* `runuser` context: when `install.sh` runs as root via sudo, all user-specific writes (icons, desktop file, systemctl --user, bookmarks) use `runuser -u $SUDO_USER`.
## Systemd and desktop integration
User-scope service (`Type=oneshot`, 1-hour `TimeoutStopSec`) and timer (`Persistent=true`, ±5-minute `RandomizedDelaySec`). Icon installed as 128×128 PNG + scalable SVG under `~/.local/share/icons/hicolor/`. Launcher written from a `@EXEC@`-substituted `.desktop` template.
## Config file
`~/.config/home-backup/config.toml` — all fields editable in the GUI. New fields added after initial install are backward-compatible via `#[serde(default)]`.
## Project layout
`src/main.rs`, `src/config.rs`, `src/drives.rs`, `src/backup.rs`, `src/systemd.rs`, `src/ui/mod.rs`, `src/ui/install.rs`, `src/ui/main_win.rs`, `assets/home-backup.{png,svg,desktop}`, `install.sh`, `README.md`.
## Build and install
`./install.sh` checks dependencies, compiles with `cargo build --release --quiet`, installs the binary, icon, launcher, and assets. Supports `--system`, `--skip-build`, and `--yes`. `./install.sh status` reports installed files and timer state. `./install.sh uninstall` removes all installed files while leaving config and backup data intact.