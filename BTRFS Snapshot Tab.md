# BTRFS Snapshot Tab
## Overview
Add an optional tab to the management window that enables BTRFS subvolume snapshots of the source directory.  The tab is only active when the source filesystem is BTRFS; on other filesystems it appears disabled with an explanatory note.  Phase 1 covers snapshot creation and recovery instructions.  Phase 2 (future) adds a GUI for browsing and restoring individual files.
## BTRFS detection
At startup and whenever the source path changes, call `findmnt -n -o FSTYPE --target <source_dir>` to read the filesystem type.  The result is cached in a new `source_fstype: Option<String>` field on the `Config` struct (not persisted; refreshed on load).  If the value is `"btrfs"` the tab is fully interactive; for all other types it is insensitive and shows a dim label: *"BTRFS snapshots are only available when the source directory is on a BTRFS filesystem."*
## Snapshot layout
Snapshots are stored as read-only BTRFS subvolumes alongside the rsync destination.  Default location: a `.btrfs-snapshots` directory inside `dest_dir`, e.g.:
```bash
/mnt/home_backups/verona1/.btrfs-snapshots/
  home-2026-07-27_020000/   ← read-only subvolume
  home-2026-07-27_141500/
```
The snapshot name is `<source_basename>-<timestamp>`.  The location is configurable in the tab.
## Snapshot creation
Creating a snapshot calls:
```bash
btrfs subvolume snapshot -r <source_dir> <snapshot_path>
```
No root is required if the user owns the subvolume.  The command is run via `std::process::Command`; stdout/stderr are captured and shown in a result label.  A spinner or progress label indicates the operation is running (BTRFS snapshots are fast — typically under one second).
## Tab layout (Phase 1)
* **Heading**: "BTRFS Snapshots" with a dim subtitle showing the detected filesystem type.
* **Snapshot path** — editable Entry showing the default `.btrfs-snapshots` path inside `dest_dir`; Browse button.
* **Create Snapshot** button — calls `btrfs subvolume snapshot -r`; shows success/error inline.
* **Existing snapshots** — a scrollable list (`gtk4::ListBox`) of snapshots found in the snapshot path, each row showing name and date.  Populated on tab switch.  A Refresh button rescans.
* **Recovery instructions** panel — a read-only `TextView` showing per-snapshot mount and restore instructions (see below).  Updates when the user selects a snapshot in the list.
* **Delete snapshot** button — calls `btrfs subvolume delete <path>`; prompts for confirmation.
## Recovery instructions text
When a snapshot is selected, the instructions panel shows:
```bash
To access this snapshot:

  1. Mount the BTRFS volume:
       sudo mount -o subvol=.btrfs-snapshots/<name> /dev/<device> /mnt/recovery

  2. Browse and copy individual files:
       ls /mnt/recovery/
       cp /mnt/recovery/Documents/file.txt ~/Documents/

  3. Unmount when done:
       sudo umount /mnt/recovery

To restore your entire home directory from this snapshot:

  WARNING: this overwrites your current home directory.
  1. Boot from a live USB or external session.
  2. Mount the BTRFS volume and delete the current subvolume:
       sudo btrfs subvolume delete /home/norm
  3. Create a writable snapshot from the recovery point:
       sudo btrfs subvolume snapshot \
         /mnt/btrfs/.btrfs-snapshots/<name> /home/norm
  4. Reboot.
```
The device path is resolved via `findmnt -n -o SOURCE --target <snapshot_dir>`.  The instructions are selectable and copyable.
## Phase 2 — Send to ext4 backup drive
`btrfs send <read-only-snapshot> | gzip -c > <dest>.btrfs.gz` on the ext4 drive; `gunzip | btrfs receive` to restore.
**Destination section** added below Phase 1 in the same tab:
* Destination path entry (default: `<dest_dir>/.btrfs-send/`)
* Snapshot selector ComboBoxText (populated from Phase 1 list)
* Parent snapshot selector (optional, for incremental sends)
* Send Snapshot button — runs in a background thread; polls result every 500 ms
* List of `.btrfs.gz` archives at the destination
* Copyable receive command shown per selected archive
* If `btrfs send` fails with permission denied, show the exact terminal command
## About tab pin
About is always the rightmost tab.  In `show()` the BTRFS tab is appended before About so About stays last.
## Files to modify
* `src/config.rs` — add `source_fstype: Option<String>` (transient, not serialised)
* `src/drives.rs` — add `detect_fstype(path: &str) -> Option<String>` via `findmnt`
* `src/ui/main_win.rs` — add `build_btrfs_tab()` function; append BTRFS tab before About; detect fstype on startup
* `README.md` — document the BTRFS tab under Main Window
## Implementation notes
* `btrfs` CLI must be present; check at tab-open time and show an error if missing (`sudo dnf install btrfs-progs`).
* All `btrfs` commands should be run as the current user; only the delete and snapshot commands may need sudo if the subvolume is owned by root.  Document this in the instructions.
* The tab is safe to ship even on non-BTRFS systems — it simply stays disabled.
