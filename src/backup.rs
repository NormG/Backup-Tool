use anyhow::{bail, Context, Result};
use chrono::Local;
use std::{
    fs,
    io::Write,
    os::unix::fs::{self as unix_fs, PermissionsExt},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, SystemTime},
};

use crate::{config::Config, drives, pending_full, run_lock};

/// Which kind of rsync backup to create.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackupKind {
    /// A fresh backup — no `--link-dest`, every file is copied.
    Full,
    /// Uses `--link-dest` pointing at the latest backup, hardlinking
    /// unchanged files so only deltas consume additional space.
    Incremental,
    /// Decide automatically: full on the configured day-of-week or when no
    /// full backup exists yet; incremental otherwise.
    Auto,
    /// The incremental period has not elapsed since the last backup;
    /// exit cleanly without creating a new backup.
    Skip,
}

impl std::str::FromStr for BackupKind {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        match s.to_ascii_lowercase().as_str() {
            "full" => Ok(Self::Full),
            "incremental" | "inc" => Ok(Self::Incremental),
            "auto" => Ok(Self::Auto),
            other => bail!("unknown backup kind '{other}'; use full, incremental, or auto"),
        }
    }
}

/// Max time an automatic backup waits for an in-progress run.
const AUTO_LOCK_WAIT_SECS: u64 = 7200;

/// Run a backup, appending progress to the application log.
///
/// Returns a human-readable summary suitable for display in the GUI.
pub fn run(config: &Config, kind: BackupKind) -> Result<String> {
    // Capture before the lock wait so a midnight crossing cannot change the plan.
    let auto_wants_full = matches!(kind, BackupKind::Auto)
        && (pending_full::pending_full_for_auto()? || is_full_backup_day(config));
    let scheduled_full_date = if matches!(kind, BackupKind::Auto) && is_full_backup_day(config) {
        Some(Local::now().format("%Y-%m-%d").to_string())
    } else {
        None
    };

    let _run_lock = match kind {
        BackupKind::Auto => {
            match run_lock::RunLock::acquire_wait(Duration::from_secs(AUTO_LOCK_WAIT_SECS))? {
                Some(lock) => lock,
                None => {
                    if auto_wants_full {
                        pending_full::set_pending_full_backup_with_retry(true).with_context(
                            || "timed out waiting for run lock while a full backup was due",
                        )?;
                    }
                    anyhow::bail!(
                        "Timed out after {} hours waiting for an in-progress backup to finish",
                        AUTO_LOCK_WAIT_SECS / 3600
                    );
                }
            }
        }
        _ => match run_lock::RunLock::try_acquire()? {
            Some(lock) => lock,
            None => {
                return Ok("Backup skipped: another backup is already in progress.".to_string());
            }
        },
    };

    // ── 1. Ensure the destination directory is reachable ──────────────────
    let dest_root = resolve_dest(config)?;
    if let Some(msg) = drives::enable_btrfs_dest_compression(&dest_root) {
        let log_path = Config::log_path();
        if let Ok(mut log) = open_log_append(&log_path) {
            let ts = Local::now().format("%Y-%m-%d %H:%M:%S");
            let _ = writeln!(log, "[{ts}] {msg}");
        }
    }
    reclaim_backup_drive_trash(&dest_root)?;
    // Leftovers from interrupted runs. We hold the run lock, so none of these
    // can belong to a backup that is still running.
    if let Ok(mut log) = open_log_append(&Config::log_path()) {
        purge_stale_inprogress(&dest_root, &mut log);
        purge_stale_rsync_logs(&Config::data_dir(), STALE_RSYNC_LOG_AGE, &mut log);
    }

    // ── 2. Determine full vs incremental ──────────────────────────────────
    let pending_since = if matches!(kind, BackupKind::Auto) {
        pending_full::pending_full_since()?
    } else {
        None
    };

    let pending_retry = matches!(kind, BackupKind::Auto) && pending_full::pending_full_for_auto()?;
    let effective_kind = match kind {
        BackupKind::Auto => auto_kind_with_pending(
            config,
            &dest_root,
            pending_retry || auto_wants_full,
            scheduled_full_date.as_deref(),
            pending_since.as_deref(),
        ),
        BackupKind::Skip => {
            return Ok("Backup skipped (called with Skip kind directly).".to_string());
        }
        other => other,
    };
    if effective_kind == BackupKind::Skip {
        let msg = format!(
            "Backup skipped (incremental period {n}d — last backup is recent enough).",
            n = config.incremental_every_n_days
        );
        return Ok(msg);
    }

    // ── 2b. Full backup: prune first, then verify there is room ───────────
    let mut deferred_full = false;
    let mut defer_space: Option<(u64, u64)> = None;
    let effective_kind = if effective_kind == BackupKind::Full {
        match prepare_full_backup(config, &dest_root)? {
            FullPrep::Proceed => BackupKind::Full,
            FullPrep::DeferredToIncremental { available, needed } => {
                deferred_full = true;
                defer_space = Some((available, needed));
                pending_full::set_pending_full_backup_with_retry(true)?;
                let msg = format!(
                    "Full backup deferred ({} free, {} needed) — running incremental instead; will retry full on next run.",
                    format_bytes(available),
                    format_bytes(needed)
                );
                let log_path = Config::log_path();
                if let Ok(mut log) = open_log_append(&log_path) {
                    let ts = Local::now().format("%Y-%m-%d %H:%M:%S");
                    let _ = writeln!(log, "[{ts}] {msg}");
                }
                BackupKind::Incremental
            }
            FullPrep::Blocked { available, needed } => {
                if let Err(e) = pending_full::set_pending_full_backup_with_retry(true) {
                    anyhow::bail!(
                        "Full backup blocked: need {} free but only {} available.\n\
                         Free space on the backup drive or remove old full-* backups, then retry.\n\
                         Could not persist deferred-full retry state: {e}",
                        format_bytes(needed),
                        format_bytes(available),
                    );
                }
                return Ok(format!(
                    "Full backup blocked: need {} free but only {} available.\n\
                     Free space on the backup drive or remove old full-* backups, then retry.",
                    format_bytes(needed),
                    format_bytes(available)
                ));
            }
        }
    } else {
        effective_kind
    };

    // ── 3. Prepare paths ──────────────────────────────────────────────────
    let stamp = Local::now().format("%Y-%m-%d_%H%M%S").to_string();
    let prefix = match effective_kind {
        BackupKind::Full => "full",
        _ => "inc",
    };
    let temp_dir = dest_root.join(format!(".inprogress-{prefix}-{stamp}"));
    let final_dir = dest_root.join(format!("{prefix}-{stamp}"));
    let latest_link = dest_root.join("latest");

    fs::create_dir_all(&temp_dir)
        .with_context(|| format!("creating staging dir {}", temp_dir.display()))?;

    // ── 4. Open log file ───────────────────────────────────────────────────
    let log_path = Config::log_path();
    let mut log_file = open_log_append(&log_path)
        .with_context(|| format!("opening log {}", log_path.display()))?;

    let log_ts = Local::now().format("%Y-%m-%d %H:%M:%S");
    writeln!(
        log_file,
        "[{log_ts}] Starting {prefix} backup → {final_dir:?}"
    )?;

    // ── 5. Write temporary exclude file ───────────────────────────────────
    let exclude_path = Config::data_dir().join("excludes.tmp");
    fs::create_dir_all(Config::data_dir())?;
    fs::write(&exclude_path, config.excludes.join("\n"))
        .context("writing temporary exclude file")?;

    // ── 6. Build rsync command ─────────────────────────────────────────────
    let mut cmd = Command::new("rsync");
    // NOTE: --inplace and --partial are intentionally omitted.
    // --link-dest creates hardlinks for unchanged files; using --inplace with
    // --link-dest can cause rsync to write new content through an existing
    // hardlink, silently corrupting the previous snapshot.
    cmd.args([
        "--archive",
        "--delete",
        "--numeric-ids",
        "--human-readable",
        "--stats",
    ]);
    cmd.arg(format!("--exclude-from={}", exclude_path.display()));
    // Per-run rsync detail goes to a temp file — not the main backup.log.
    let rsync_log = Config::data_dir().join(format!("rsync-{stamp}.log"));
    cmd.arg(format!("--log-file={}", rsync_log.display()));

    // Hardlink unchanged files from the latest snapshot. A full backup also
    // re-verifies every file by checksum, but still only writes what changed,
    // so it never needs a second complete copy on the backup drive.
    let latest = resolve_latest(&dest_root);
    let link_args = link_dest_args(effective_kind, latest.as_deref());
    if effective_kind == BackupKind::Full && !link_args.is_empty() {
        writeln!(
            log_file,
            "[{log_ts}] Full backup is hardlinked to the latest snapshot and \
             checksum-verified (only changed data is written)"
        )?;
    }
    cmd.args(&link_args);

    let source = format!("{}/", config.source_dir);
    cmd.arg(&source);
    cmd.arg(&temp_dir);

    // Inherit stderr so journal captures it when called by systemd.
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::inherit());

    // ── 7. Execute ────────────────────────────────────────────────────────
    let child = cmd.spawn().context("spawning rsync")?;
    let output = child.wait_with_output().context("waiting for rsync")?;

    let exit_code = output.status.code().unwrap_or(-1);
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();

    // rsync exit 24 = source files vanished (normal for live home dirs).
    if !output.status.success() && exit_code != 24 {
        make_tree_deletable(&temp_dir);
        let _ = fs::remove_dir_all(&temp_dir);
        let _ = fs::remove_file(&rsync_log);
        bail!("rsync failed with exit code {exit_code}");
    }

    if exit_code == 24 {
        writeln!(
            log_file,
            "[{log_ts}] WARNING: exit 24 — some files vanished during transfer (non-fatal)"
        )?;
    }
    let _ = fs::remove_file(&rsync_log);

    // ── 8. Atomic rename & update latest symlink ──────────────────────────
    fs::rename(&temp_dir, &final_dir)
        .with_context(|| format!("renaming {temp_dir:?} → {final_dir:?}"))?;

    // Remove old symlink and re-create it atomically.
    if latest_link.is_symlink() || latest_link.exists() {
        fs::remove_file(&latest_link).ok();
    }
    unix_fs::symlink(&final_dir, &latest_link)
        .with_context(|| format!("creating latest symlink → {final_dir:?}"))?;

    writeln!(
        log_file,
        "[{log_ts}] Completed {prefix} backup: {}",
        final_dir.file_name().unwrap_or_default().to_string_lossy()
    )?;

    // ── 9. Weekly log rotation (full backups only) ────────────────────────
    if effective_kind == BackupKind::Full {
        match archive_and_rotate_log(&final_dir) {
            Ok(fresh) => log_file = fresh,
            Err(e) => log_line(
                &mut log_file,
                &format!("WARNING: log rotation failed (backup succeeded): {e}"),
            ),
        }
    }

    // ── 10. Retention ─────────────────────────────────────────────────────
    if effective_kind == BackupKind::Full {
        // Current-cycle incrementals are superseded by the new full.
        prune_all_incrementals(&dest_root, &mut log_file)?;
        apply_full_retention(&dest_root, config.keep_full_snapshots, &mut log_file)?;
        pending_full::clear_pending_after_success().with_context(|| {
            "full backup succeeded but deferred-full state could not be cleared; \
             next automatic run may schedule another full"
        })?;
    } else {
        // Safety net for orphaned incrementals when a full has not run recently.
        apply_retention(&dest_root, config.retention_days, &mut log_file)?;
    }

    // ── 11. Desktop notification (best-effort) ─────────────────────────────
    if deferred_full {
        let (available, needed) = defer_space.unwrap_or((0, 0));
        let body = if available > 0 || needed > 0 {
            format!(
                "Scheduled full backup could not run ({available} free, {needed} required).\n\
                 Incremental backup ran instead — this is a deviation from your schedule.\n\
                 Free space on the backup drive or remove old full backups.",
                available = format_bytes(available),
                needed = format_bytes(needed),
            )
        } else {
            "Scheduled full backup could not run (insufficient space).\n\
             Incremental backup ran instead — this is a deviation from your schedule."
                .to_string()
        };
        let _ = notify_deviation("Backup-Tool — schedule deviation", &body);
    } else {
        let _ = notify_success(
            "Backup-Tool",
            &format!("{prefix} backup created successfully"),
        );
    }

    // Build summary string for the GUI.
    let mut summary = build_summary(prefix, &final_dir, &stdout);
    if deferred_full {
        summary = format!("⚠️  Scheduled full backup deferred (insufficient space).\n\n{summary}");
    }
    Ok(summary)
}

/// Permanently delete all `inc-*` backups under the destination (never trash).
pub fn vaporize_incrementals(config: &Config) -> Result<String> {
    let dest_root = resolve_dest(config)?;
    let log_path = Config::log_path();
    let mut log = open_log_append(&log_path)?;
    let ts = Local::now().format("%Y-%m-%d %H:%M:%S");
    writeln!(
        log,
        "[{ts}] Vaporizing all incremental backups under {}",
        dest_root.display()
    )?;
    prune_all_incrementals(&dest_root, &mut log)?;
    Ok(format!(
        "Vaporized all inc-* backups under {} (permanent delete, not trash).",
        dest_root.display()
    ))
}

/// Permanently empty freedesktop trash on the backup volume (Nautilus deletes, etc.).
pub fn vaporize_volume_trash(config: &Config) -> Result<String> {
    let dest_root = resolve_dest(config)?;
    let freed = drives::empty_volume_trash(&dest_root)?;
    let log_path = Config::log_path();
    if let Ok(mut log) = open_log_append(&log_path) {
        let ts = Local::now().format("%Y-%m-%d %H:%M:%S");
        let _ = writeln!(
            log,
            "[{ts}] Vaporized backup-volume trash ({} freed)",
            format_bytes(freed)
        );
    }
    Ok(format!(
        "Vaporized backup-volume trash ({} freed).",
        format_bytes(freed)
    ))
}

// ── Helpers ──────────────────────────────────────────────────────────────────

/// Extra rsync arguments that tie a new snapshot to the previous one.
///
/// - Incremental: `--link-dest` to the latest snapshot.
/// - Full with a previous snapshot: the same, plus `--checksum` so every file
///   is re-read and compared by content (catching silent corruption of the
///   hardlinked copy) while unchanged data stays hardlinked.
/// - No previous snapshot: nothing, i.e. a plain complete copy.
fn link_dest_args(kind: BackupKind, latest: Option<&Path>) -> Vec<String> {
    let Some(link) = latest else {
        return Vec::new();
    };
    let mut args = vec![format!("--link-dest={}", link.display())];
    if kind == BackupKind::Full {
        args.push("--checksum".to_string());
    }
    args
}

/// A hardlinked full backup only writes files that changed since the latest
/// snapshot, so it needs about as much room as an incremental. Reserve this
/// fraction of the volume as headroom for that delta, but never less than
/// [`HARDLINK_FULL_MIN_BYTES`] -- and never more than
/// [`HARDLINK_FULL_MIN_FRACTION`] of the volume, so small drives (where a fixed
/// 10 GiB floor would exceed the whole drive) can still complete fulls.
const HARDLINK_FULL_HEADROOM_FRACTION: f64 = 0.02;
const HARDLINK_FULL_MIN_BYTES: u64 = 10 * 1024 * 1024 * 1024;
const HARDLINK_FULL_MIN_FRACTION: f64 = 0.10;

/// Minimum free space to start the very first (non-hardlinked) full backup.
const FRESH_FULL_MIN_BYTES: u64 = 1024 * 1024 * 1024;

enum FullPrep {
    Proceed,
    DeferredToIncremental { available: u64, needed: u64 },
    Blocked { available: u64, needed: u64 },
}

/// Before a full backup: verify the drive has enough free space.  Incrementals
/// are removed only after the full succeeds — never during preparation.
fn prepare_full_backup(_config: &Config, dest_root: &Path) -> Result<FullPrep> {
    let log_path = Config::log_path();
    let mut log = open_log_append(&log_path)?;

    let (available, volume_total) = drives::filesystem_bytes(dest_root)?;
    let needed = full_backup_bytes_needed(dest_root, volume_total);

    let ts = Local::now().format("%Y-%m-%d %H:%M:%S");
    if available >= needed {
        writeln!(
            log,
            "[{ts}] Space check passed: {} free, {} required",
            format_bytes(available),
            format_bytes(needed)
        )?;
        return Ok(FullPrep::Proceed);
    }

    writeln!(
        log,
        "[{ts}] Space check failed: {} free, {} required",
        format_bytes(available),
        format_bytes(needed)
    )?;

    if resolve_latest(dest_root).is_some() {
        Ok(FullPrep::DeferredToIncremental { available, needed })
    } else {
        Ok(FullPrep::Blocked { available, needed })
    }
}

/// Bytes of free space required to start a full backup.
///
/// With a previous snapshot the full is hardlinked (see [`link_dest_args`]) and
/// only needs room for changed data; the old estimate (110% of the latest
/// snapshot's apparent size, ignoring both compression and hardlinks) demanded
/// a second complete copy and could never pass on a drive this full.
fn full_backup_bytes_needed(dest_root: &Path, volume_total: u64) -> u64 {
    if resolve_latest(dest_root).is_some() {
        hardlinked_full_bytes_needed(volume_total)
    } else {
        FRESH_FULL_MIN_BYTES
    }
}

fn hardlinked_full_bytes_needed(volume_total: u64) -> u64 {
    let headroom = (volume_total as f64 * HARDLINK_FULL_HEADROOM_FRACTION).ceil() as u64;
    let floor = HARDLINK_FULL_MIN_BYTES
        .min((volume_total as f64 * HARDLINK_FULL_MIN_FRACTION).ceil() as u64);
    headroom.max(floor)
}

/// Rsync logs older than this are leftovers (a successful or failed run
/// deletes its own log); the age gate keeps this run's log out of reach.
const STALE_RSYNC_LOG_AGE: Duration = Duration::from_secs(24 * 60 * 60);

/// True if a command-line argument refers to the staging directory called
/// `name`: the argument is that directory, or a path inside it (rsync is given
/// the staging directory as its destination).
fn arg_refers_to_dir(arg: &str, name: &str) -> bool {
    arg.match_indices(name).any(|(i, _)| {
        let end = i + name.len();
        let before_ok = i == 0 || arg.as_bytes()[i - 1] == b'/';
        let after_ok = end == arg.len() || arg.as_bytes()[end] == b'/';
        before_ok && after_ok
    })
}

/// PIDs of running processes whose command line refers to the staging
/// directory `name`. Fails if `/proc` cannot be listed.
fn processes_using_dir(name: &str) -> std::io::Result<Vec<u32>> {
    let mut pids = Vec::new();
    for entry in fs::read_dir("/proc")? {
        let entry = entry?;
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<u32>().ok())
        else {
            continue;
        };
        // The process may exit while we scan, or be unreadable: skip it.
        let Ok(raw) = fs::read(entry.path().join("cmdline")) else {
            continue;
        };
        let uses_dir = raw
            .split(|b| *b == 0)
            .any(|arg| std::str::from_utf8(arg).is_ok_and(|a| arg_refers_to_dir(a, name)));
        if uses_dir {
            pids.push(pid);
        }
    }
    Ok(pids)
}

/// Remove `.inprogress-*` staging directories left in `dest_root` by an
/// interrupted backup (power loss, `systemctl stop`, kill).
///
/// A staging directory is renamed on success and removed when rsync fails, so
/// one that is still there was normally abandoned. The run lock is NOT proof of
/// that: if `backup-tool` is killed while its rsync child keeps running, the
/// lock is released but rsync is still writing into the directory. So a
/// directory is skipped while any running process still refers to it, and if
/// `/proc` cannot be inspected nothing is deleted. Only real directories that
/// are direct children of `dest_root` are touched; symlinks and files with that
/// prefix are left alone. Failures are logged, never fatal.
/// Returns how many directories were removed.
fn purge_stale_inprogress(dest_root: &Path, log: &mut fs::File) -> usize {
    let Ok(entries) = dest_root.read_dir() else {
        return 0;
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let n = name.to_string_lossy();
        if !n.starts_with(".inprogress-") {
            continue;
        }
        let path = entry.path();
        // symlink_metadata: a symlink is not a directory here, so it is skipped.
        let Ok(meta) = fs::symlink_metadata(&path) else {
            continue;
        };
        if !meta.is_dir() {
            continue;
        }
        match processes_using_dir(&n) {
            Ok(pids) if !pids.is_empty() => {
                log_line(
                    log,
                    &format!(
                        "Keeping staging directory {n}: still in use by process(es) {pids:?} \
                         (an interrupted backup whose rsync is still running?)"
                    ),
                );
                continue;
            }
            Ok(_) => {}
            Err(e) => {
                log_line(
                    log,
                    &format!(
                        "WARNING: cannot check /proc for running processes ({e}); \
                         not removing stale staging directories this run"
                    ),
                );
                return removed;
            }
        }
        make_tree_deletable(&path);
        match fs::remove_dir_all(&path) {
            Ok(()) => {
                log_line(
                    log,
                    &format!("Removed stale staging directory {n} (left by an interrupted backup)"),
                );
                removed += 1;
            }
            Err(e) => log_line(
                log,
                &format!("WARNING: could not remove stale staging directory {n}: {e}"),
            ),
        }
    }
    removed
}

/// Remove `rsync-*.log` files in `data_dir` that are at least `min_age` old.
/// Only regular files named `rsync-*.log` are touched. Failures are ignored.
/// Returns how many files were removed.
fn purge_stale_rsync_logs(data_dir: &Path, min_age: Duration, log: &mut fs::File) -> usize {
    let Ok(entries) = data_dir.read_dir() else {
        return 0;
    };
    let now = SystemTime::now();
    let mut removed = 0;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let n = name.to_string_lossy();
        if !(n.starts_with("rsync-") && n.ends_with(".log")) {
            continue;
        }
        let path = entry.path();
        let Ok(meta) = fs::symlink_metadata(&path) else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        let Ok(modified) = meta.modified() else {
            continue;
        };
        let old_enough = now.duration_since(modified).is_ok_and(|age| age >= min_age);
        if old_enough && fs::remove_file(&path).is_ok() {
            log_line(
                log,
                &format!("Removed stale rsync log {n} (left by an interrupted backup)"),
            );
            removed += 1;
        }
    }
    removed
}

/// Empty trash on the backup volume; log and continue on failure.
fn reclaim_backup_drive_trash(dest_root: &Path) -> Result<()> {
    match drives::empty_volume_trash(dest_root) {
        Ok(freed) if freed > 0 => {
            let log_path = Config::log_path();
            if let Ok(mut log) = open_log_append(&log_path) {
                log_line(
                    &mut log,
                    &format!("Emptied backup-drive trash ({} freed)", format_bytes(freed)),
                );
            }
        }
        Ok(_) => {}
        Err(e) => {
            let log_path = Config::log_path();
            if let Ok(mut log) = open_log_append(&log_path) {
                log_line(
                    &mut log,
                    &format!("WARNING: could not empty backup-drive trash: {e}"),
                );
            }
        }
    }
    Ok(())
}

fn open_log_append(log_path: &Path) -> Result<fs::File> {
    if let Some(p) = log_path.parent() {
        fs::create_dir_all(p)?;
    }
    fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
        .with_context(|| format!("opening log {}", log_path.display()))
}

fn log_line(log: &mut fs::File, message: &str) {
    let ts = Local::now().format("%Y-%m-%d %H:%M:%S");
    let _ = writeln!(log, "[{ts}] {message}");
}

fn try_remove_snapshot(path: &Path, name: &str, log: &mut fs::File, context: &str) {
    make_tree_deletable(path);
    if let Err(e) = fs::remove_dir_all(path) {
        log_line(
            log,
            &format!("WARNING: could not remove {context} {name}: {e}"),
        );
    } else {
        log_line(log, &format!("Vaporized {name} ({context})"));
    }
}

/// Add owner rwx on directories so `remove_dir_all` can list and unlink
/// children. `rsync --archive` preserves modes; Go module caches (`$GOPATH/pkg/mod`)
/// are 0555 by design. Does not follow symlinks.
fn make_tree_deletable(root: &Path) {
    let Ok(meta) = fs::symlink_metadata(root) else {
        return;
    };
    if meta.file_type().is_symlink() || !meta.is_dir() {
        return;
    }
    let mut perms = meta.permissions();
    perms.set_mode(perms.mode() | 0o700);
    let _ = fs::set_permissions(root, perms);
    if let Ok(entries) = fs::read_dir(root) {
        for entry in entries.flatten() {
            make_tree_deletable(&entry.path());
        }
    }
}

/// Copy the active backup log into a full snapshot, then truncate it for the
/// new backup cycle.  Archived logs live at `{snapshot}/.backup-tool/backup.log`.
fn archive_and_rotate_log(snapshot_dir: &Path) -> Result<fs::File> {
    let log_path = Config::log_path();
    let archive_path = archive_log_to_snapshot(&log_path, snapshot_dir)?;

    fs::write(&log_path, "").with_context(|| format!("truncating log {}", log_path.display()))?;

    let mut fresh = open_log_append(&log_path)?;
    let ts = Local::now().format("%Y-%m-%d %H:%M:%S");
    if let Some(archived) = archive_path {
        writeln!(
            fresh,
            "[{ts}] Log rotated — previous cycle archived to {}",
            archived.display()
        )?;
    } else {
        writeln!(fresh, "[{ts}] Log rotated — starting fresh backup cycle")?;
    }
    Ok(fresh)
}

fn archive_log_to_snapshot(active_log: &Path, snapshot_dir: &Path) -> Result<Option<PathBuf>> {
    if !active_log.exists() {
        return Ok(None);
    }

    let raw = fs::read_to_string(active_log)
        .with_context(|| format!("reading log {}", active_log.display()))?;
    // Keep operational lines only — rsync per-file detail is discarded.
    let trimmed: String = raw
        .lines()
        .filter(|line| line.starts_with('['))
        .collect::<Vec<_>>()
        .join("\n");
    if trimmed.is_empty() {
        return Ok(None);
    }

    let archive_dir = snapshot_dir.join(".backup-tool");
    fs::create_dir_all(&archive_dir)
        .with_context(|| format!("creating log archive dir {}", archive_dir.display()))?;
    let archive_path = archive_dir.join("backup.log");
    fs::write(&archive_path, format!("{trimmed}\n"))
        .with_context(|| format!("writing archived log to {}", archive_path.display()))?;
    Ok(Some(archive_path))
}

fn format_bytes(n: u64) -> String {
    const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
    const MIB: f64 = 1024.0 * 1024.0;
    const KIB: f64 = 1024.0;
    let n = n as f64;
    if n >= GIB {
        format!("{:.1} GiB", n / GIB)
    } else if n >= MIB {
        format!("{:.1} MiB", n / MIB)
    } else if n >= KIB {
        format!("{:.1} KiB", n / KIB)
    } else {
        format!("{n:.0} B")
    }
}

/// Ensure the backup destination is accessible, trying auto-mount when
/// needed.  Returns the resolved `PathBuf`.
fn resolve_dest(config: &Config) -> Result<PathBuf> {
    let dest = Path::new(&config.dest_dir);

    // Check if destination already exists
    if dest.exists() {
        return Ok(dest.to_path_buf());
    }

    // If not, check if the drive is already mounted by UUID
    if let Some(uuid) = &config.drive_uuid {
        // Check if drive is already mounted
        if let Some(mp) = drives::find_mountpoint_by_uuid(uuid) {
            // Drive is already mounted, check if our destination path exists
            let candidate = PathBuf::from(&mp).join(dest.strip_prefix("/").unwrap_or(dest));
            if candidate.exists() {
                return Ok(dest.to_path_buf());
            }
            // Mount succeeded but subdir missing — create it.
            fs::create_dir_all(dest)
                .with_context(|| format!("creating dest dir {}", dest.display()))?;
            return Ok(dest.to_path_buf());
        }

        // Drive is not mounted, so mount it
        let _mp =
            drives::mount_by_uuid(uuid).with_context(|| format!("auto-mounting UUID {uuid}"))?;

        fs::create_dir_all(dest).with_context(|| "creating dest dir after mount")?;
        if dest.exists() {
            return Ok(dest.to_path_buf());
        }
    }

    bail!(
        "Backup destination '{}' is not accessible and could not be auto-mounted.\n\
         Please plug in the backup drive and try again.",
        config.dest_dir
    )
}

fn is_full_backup_day(config: &Config) -> bool {
    let today = Local::now().format("%A").to_string();
    today.eq_ignore_ascii_case(&config.full_backup_day)
}

fn full_snapshot_exists_on_date(dest_root: &Path, date: &str) -> bool {
    dest_root
        .read_dir()
        .map(|rd| {
            rd.flatten().any(|e| {
                let file_name = e.file_name();
                let name = file_name.to_string_lossy();
                name.starts_with("full-") && name.contains(date)
            })
        })
        .unwrap_or(false)
}

fn full_snapshot_exists_today(dest_root: &Path) -> bool {
    let today = Local::now().format("%Y-%m-%d").to_string();
    full_snapshot_exists_on_date(dest_root, &today)
}

fn full_snapshot_already_satisfied(dest_root: &Path, scheduled_full_date: Option<&str>) -> bool {
    if full_snapshot_exists_today(dest_root) {
        return true;
    }
    scheduled_full_date.is_some_and(|d| full_snapshot_exists_on_date(dest_root, d))
}

/// Pending full is satisfied when a full snapshot newer than [`pending_since`]
/// exists and no incremental snapshot is newer than that full.
fn pending_full_already_resolved(dest_root: &Path, pending_since: Option<&str>) -> bool {
    let since = match pending_since {
        Some(s) => s,
        None => return false,
    };
    let latest_full = match newest_full_snapshot(dest_root) {
        Some(p) => p,
        None => return false,
    };
    let full_name = latest_full
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    let full_ts = match snapshot_timestamp(&full_name) {
        Some(ts) if ts.as_str() >= since => ts,
        _ => return false,
    };

    dest_root
        .read_dir()
        .map(|rd| {
            !rd.flatten().any(|e| {
                let file_name = e.file_name();
                let name = file_name.to_string_lossy();
                name.starts_with("inc-") && snapshot_timestamp(&name).is_some_and(|ts| ts > full_ts)
            })
        })
        .unwrap_or(false)
}

fn newest_full_snapshot(dest_root: &Path) -> Option<PathBuf> {
    dest_root
        .read_dir()
        .ok()?
        .flatten()
        .filter(|e| {
            let file_name = e.file_name();
            let name = file_name.to_string_lossy();
            name.starts_with("full-") && e.path().is_dir()
        })
        .max_by_key(|e| e.file_name())
        .map(|e| e.path())
}

fn snapshot_timestamp(name: &str) -> Option<String> {
    let dash = name.find('-')?;
    Some(name.get(dash + 1..)?.to_string())
}

/// Decide the backup kind based on the day of week, existing snapshots,
/// and the configured incremental period.
#[cfg(test)]
fn auto_kind(config: &Config, dest_root: &Path, pending_retry: bool) -> Result<BackupKind> {
    Ok(auto_kind_with_pending(
        config,
        dest_root,
        pending_retry,
        None,
        None,
    ))
}

fn auto_kind_with_pending(
    config: &Config,
    dest_root: &Path,
    pending_full: bool,
    scheduled_full_date: Option<&str>,
    pending_since: Option<&str>,
) -> BackupKind {
    if pending_full {
        if full_snapshot_already_satisfied(dest_root, scheduled_full_date)
            || pending_full_already_resolved(dest_root, pending_since)
        {
            // Full already landed (e.g. clear failed after success) — heal state.
            let _ = pending_full::clear_pending_after_success();
        } else {
            return BackupKind::Full;
        }
    }

    let today = Local::now().format("%A").to_string(); // "Monday", "Tuesday", …
    let is_full_day = today.eq_ignore_ascii_case(&config.full_backup_day);

    let full_exists = dest_root
        .read_dir()
        .map(|rd| {
            rd.flatten()
                .any(|e| e.file_name().to_string_lossy().starts_with("full-"))
        })
        .unwrap_or(false);

    if !full_exists {
        return BackupKind::Full;
    }

    if is_full_day && !full_snapshot_already_satisfied(dest_root, scheduled_full_date) {
        return BackupKind::Full;
    }

    // Check whether the incremental period has elapsed since the last snapshot.
    // Snapshot names: {full,inc}-YYYY-MM-DD_HHmmss
    //
    // NOTE: the guard applies to ALL periods including 1.  With period=1 this
    // means "at most one incremental per calendar day", which prevents both
    // duplicate snapshots from multiple same-day manual invocations and a
    // timestamp-collision rename failure when two runs happen in the same second.
    let period = config.incremental_every_n_days.max(1);
    if let Some(latest) = resolve_latest(dest_root) {
        let name = latest
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        // Snapshot names: "full-YYYY-MM-DD_HHmmss" or "inc-YYYY-MM-DD_HHmmss".
        // "full-" is 5 chars; "inc-" is 4 chars.  Find the first '-' to locate
        // the date regardless of prefix length.
        if let Some(prefix_end) = name.find('-') {
            let date_start = prefix_end + 1;
            let date_end = date_start + 10; // "YYYY-MM-DD" is always 10 chars
            if let Some(date_str) = name.get(date_start..date_end) {
                if let Ok(date) = chrono::NaiveDate::parse_from_str(date_str, "%Y-%m-%d") {
                    let today_date = Local::now().date_naive();
                    let days_since = (today_date - date).num_days();
                    if days_since < i64::from(period) {
                        return BackupKind::Skip;
                    }
                }
            }
        }
    }
    BackupKind::Incremental
}

/// Find the most-recently-modified snapshot directory (full or inc) under
/// `dest_root`, for use as `--link-dest`.
fn resolve_latest(dest_root: &Path) -> Option<PathBuf> {
    let mut entries: Vec<_> = dest_root
        .read_dir()
        .ok()?
        .flatten()
        .filter(|e| {
            let name = e.file_name();
            let n = name.to_string_lossy();
            (n.starts_with("full-") || n.starts_with("inc-")) && e.path().is_dir()
        })
        .collect();

    // Sort by name descending (timestamps are sortable).
    entries.sort_by_key(|e| std::cmp::Reverse(e.file_name()));
    entries.first().map(|e| e.path())
}

/// Delete all incremental snapshots under `dest_root`.
fn prune_all_incrementals(dest_root: &Path, log: &mut fs::File) -> Result<()> {
    let entries = match dest_root.read_dir() {
        Ok(rd) => rd,
        Err(_) => return Ok(()),
    };

    for entry in entries.flatten() {
        let name = entry.file_name();
        let n = name.to_string_lossy();
        if !n.starts_with("inc-") || !entry.path().is_dir() {
            continue;
        }
        try_remove_snapshot(&entry.path(), &n, log, "incremental backup");
    }
    Ok(())
}

/// Keep only the `keep` most recent full snapshots (0 = unlimited).
fn apply_full_retention(dest_root: &Path, keep: u32, log: &mut fs::File) -> Result<()> {
    if keep == 0 {
        return Ok(());
    }

    let mut fulls: Vec<_> = match dest_root.read_dir() {
        Ok(rd) => rd
            .flatten()
            .filter(|e| e.path().is_dir() && e.file_name().to_string_lossy().starts_with("full-"))
            .map(|e| e.path())
            .collect(),
        Err(_) => return Ok(()),
    };

    fulls.sort_by_key(|p| std::cmp::Reverse(p.file_name().unwrap_or_default().to_os_string()));

    for path in fulls
        .into_iter()
        .skip(usize::try_from(keep).unwrap_or(usize::MAX))
    {
        let name = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        try_remove_snapshot(&path, &name, log, "old full backup");
    }
    Ok(())
}

/// Delete incremental snapshots older than `retention_days`.
fn apply_retention(dest_root: &Path, retention_days: u32, log: &mut fs::File) -> Result<()> {
    let cutoff = Local::now()
        .checked_sub_signed(chrono::Duration::days(i64::from(retention_days)))
        .unwrap_or_else(Local::now);
    let cutoff_sys: std::time::SystemTime = cutoff.into();

    let entries = match dest_root.read_dir() {
        Ok(rd) => rd,
        Err(_) => return Ok(()),
    };

    for entry in entries.flatten() {
        let name = entry.file_name();
        let n = name.to_string_lossy();
        if !n.starts_with("inc-") {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        let Ok(modified) = meta.modified() else {
            continue;
        };
        if modified < cutoff_sys {
            try_remove_snapshot(&entry.path(), &n, log, "old backup");
        }
    }
    Ok(())
}

/// Send a desktop notification for a successful expected backup.
fn notify_success(title: &str, body: &str) -> Result<()> {
    Command::new("notify-send")
        .args([
            "-a",
            "Backup-Tool",
            "-u",
            "normal",
            "-t",
            "5000",
            title,
            body,
        ])
        .status()?;
    Ok(())
}

/// App icon PNG for notify-send (RPM hicolor theme or assets/ next to binary).
fn app_icon_path() -> Option<PathBuf> {
    const HICOLOR: &str = "/usr/share/icons/hicolor/128x128/apps/backup-tool.png";
    if Path::new(HICOLOR).is_file() {
        return Some(PathBuf::from(HICOLOR));
    }
    const SHARE_ASSETS: &str = "/usr/share/backup-tool/assets/backup-tool.png";
    if Path::new(SHARE_ASSETS).is_file() {
        return Some(PathBuf::from(SHARE_ASSETS));
    }
    if let Ok(exe) = std::env::current_exe() {
        let next_to_binary = exe
            .parent()
            .map(|d| d.join("assets/backup-tool.png"))
            .filter(|p| p.is_file());
        if next_to_binary.is_some() {
            return next_to_binary;
        }
    }
    let dev = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("assets/backup-tool.png");
    dev.is_file().then_some(dev)
}

/// Send a high-visibility notification when reality deviates from the schedule.
fn notify_deviation(title: &str, body: &str) -> Result<()> {
    let mut cmd = Command::new("notify-send");
    cmd.args(["-a", "Backup-Tool", "-u", "critical", "-t", "30000"]);
    match app_icon_path() {
        Some(icon) => {
            cmd.arg("-i").arg(icon);
        }
        None => {
            cmd.arg("-i").arg("backup-tool");
        }
    }
    cmd.arg(title).arg(body);
    cmd.status()?;
    Ok(())
}

fn build_summary(prefix: &str, final_dir: &Path, rsync_stdout: &str) -> String {
    let name = final_dir
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();

    // Extract a few key lines from rsync --stats output.
    let stats: String = rsync_stdout
        .lines()
        .filter(|l| {
            l.contains("Number of files")
                || l.contains("Number of created files")
                || l.contains("Number of deleted files")
                || l.contains("Total file size")
                || l.contains("Total transferred")
                || l.contains("Literal data")
        })
        .collect::<Vec<_>>()
        .join("\n");

    format!("✅  {prefix} backup complete\nBackup: {name}\n\n{stats}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    // ── helpers ───────────────────────────────────────────────────────────────

    /// Create a unique temp directory for a test and return its path.
    /// The caller is responsible for removing it afterwards.
    fn tmp_dir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("backup-tool-test-{tag}-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn make_snapshot_dir(root: &Path, name: &str) {
        fs::create_dir_all(root.join(name)).unwrap();
    }

    // ── BackupKind::from_str ──────────────────────────────────────────────────

    #[test]
    fn backup_kind_parses_full() {
        for s in &["full", "Full", "FULL"] {
            assert_eq!(s.parse::<BackupKind>().unwrap(), BackupKind::Full);
        }
    }

    #[test]
    fn backup_kind_parses_incremental() {
        for s in &["incremental", "Incremental", "inc", "INC"] {
            assert_eq!(s.parse::<BackupKind>().unwrap(), BackupKind::Incremental);
        }
    }

    #[test]
    fn backup_kind_parses_auto() {
        for s in &["auto", "Auto", "AUTO"] {
            assert_eq!(s.parse::<BackupKind>().unwrap(), BackupKind::Auto);
        }
    }

    #[test]
    fn backup_kind_rejects_unknown_values() {
        for s in &["bad", "", "manual", "skip", "differential"] {
            assert!(s.parse::<BackupKind>().is_err(), "should reject '{s}'");
        }
    }

    // ── format_bytes ─────────────────────────────────────────────────────────

    #[test]
    fn format_bytes_sub_kib() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(1), "1 B");
        assert_eq!(format_bytes(1023), "1023 B");
    }

    #[test]
    fn app_icon_path_exists_in_repo() {
        assert!(
            app_icon_path().is_some(),
            "assets/backup-tool.png must ship with the project"
        );
    }

    #[test]
    fn format_bytes_kib() {
        assert_eq!(format_bytes(1024), "1.0 KiB");
        assert_eq!(format_bytes(1536), "1.5 KiB");
        assert_eq!(format_bytes(1024 * 1024 - 1), "1024.0 KiB");
    }

    #[test]
    fn format_bytes_mib() {
        assert_eq!(format_bytes(1024 * 1024), "1.0 MiB");
        assert_eq!(format_bytes(10 * 1024 * 1024), "10.0 MiB");
    }

    #[test]
    fn format_bytes_gib() {
        assert_eq!(format_bytes(1024 * 1024 * 1024), "1.0 GiB");
        assert_eq!(format_bytes(100 * 1024 * 1024 * 1024), "100.0 GiB");
    }

    // ── resolve_latest ────────────────────────────────────────────────────────

    #[test]
    fn resolve_latest_empty_dir_returns_none() {
        let dir = tmp_dir("latest-empty");
        assert!(resolve_latest(&dir).is_none());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn resolve_latest_ignores_inprogress_dirs() {
        let dir = tmp_dir("latest-inprogress");
        make_snapshot_dir(&dir, ".inprogress-full-2024-01-01_120000");
        assert!(resolve_latest(&dir).is_none());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn resolve_latest_returns_lexicographically_last() {
        let dir = tmp_dir("latest-order");
        make_snapshot_dir(&dir, "full-2024-01-01_120000");
        make_snapshot_dir(&dir, "inc-2024-01-02_120000");
        make_snapshot_dir(&dir, "inc-2024-01-03_090000"); // latest by name
        make_snapshot_dir(&dir, ".inprogress-inc-2024-01-04_120000"); // ignored

        let latest = resolve_latest(&dir).unwrap();
        assert_eq!(
            latest.file_name().unwrap().to_string_lossy(),
            "inc-2024-01-03_090000"
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    // ── auto_kind ─────────────────────────────────────────────────────────────

    #[test]
    fn auto_kind_full_when_pending_full_backup() {
        let dir = tmp_dir("auto-pending-full");
        make_snapshot_dir(&dir, "full-2020-01-01_120000");
        make_snapshot_dir(&dir, "inc-2024-01-01_120000");

        let cfg = Config {
            full_backup_day: "Neverday".to_string(),
            ..Config::default()
        };
        assert_eq!(
            auto_kind_with_pending(&cfg, &dir, true, None, None),
            BackupKind::Full
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn auto_kind_skips_pending_when_full_already_exists_today() {
        let dir = tmp_dir("auto-pending-today-full");
        let today = Local::now().format("%Y-%m-%d").to_string();
        make_snapshot_dir(&dir, &format!("full-{today}_120000"));

        let cfg = Config {
            full_backup_day: "Neverday".to_string(),
            ..Config::default()
        };
        assert_ne!(
            auto_kind_with_pending(&cfg, &dir, true, None, None),
            BackupKind::Full
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn auto_kind_skips_pending_when_scheduled_full_date_snapshot_exists() {
        let dir = tmp_dir("auto-pending-scheduled-date");
        let scheduled = "2024-06-15";
        make_snapshot_dir(&dir, &format!("full-{scheduled}_120000"));

        let cfg = Config {
            full_backup_day: "Neverday".to_string(),
            ..Config::default()
        };
        assert_ne!(
            auto_kind_with_pending(&cfg, &dir, true, Some(scheduled), None),
            BackupKind::Full
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn auto_kind_skips_pending_when_no_inc_after_latest_full() {
        let dir = tmp_dir("auto-pending-no-inc-after-full");
        make_snapshot_dir(&dir, "inc-2020-01-01_120000");
        make_snapshot_dir(&dir, "full-2024-06-15_120000");

        let cfg = Config {
            full_backup_day: "Neverday".to_string(),
            ..Config::default()
        };
        assert_ne!(
            auto_kind_with_pending(&cfg, &dir, true, None, Some("2024-06-15_110000")),
            BackupKind::Full
        );

        make_snapshot_dir(&dir, "inc-2024-06-16_120000");
        assert_eq!(
            auto_kind_with_pending(&cfg, &dir, true, None, Some("2024-06-15_110000")),
            BackupKind::Full
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn auto_kind_skips_pending_when_full_matches_pending_since_second() {
        let dir = tmp_dir("auto-pending-same-second");
        make_snapshot_dir(&dir, "full-2024-06-15_120000");

        let cfg = Config {
            full_backup_day: "Neverday".to_string(),
            ..Config::default()
        };
        assert_ne!(
            auto_kind_with_pending(&cfg, &dir, true, None, Some("2024-06-15_120000")),
            BackupKind::Full
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn auto_kind_full_when_no_full_snapshot_exists() {
        let dir = tmp_dir("auto-no-full");
        // Only inc snapshots present — no full-* directory.
        make_snapshot_dir(&dir, "inc-2024-01-01_120000");

        let cfg = Config {
            full_backup_day: "Neverday".to_string(), // won't match any real weekday
            ..Config::default()
        };
        assert_eq!(auto_kind(&cfg, &dir, false).unwrap(), BackupKind::Full);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn auto_kind_skips_when_recent_incremental_within_period() {
        let dir = tmp_dir("auto-skip");
        // A full snapshot must exist so the "no full" branch does not fire.
        make_snapshot_dir(&dir, "full-2020-01-01_120000");
        // Create an incremental dated today.
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();
        make_snapshot_dir(&dir, &format!("inc-{today}_120000"));
        // Also create the `latest` symlink pointing at the inc.
        let inc_path = dir.join(format!("inc-{today}_120000"));
        let _ = std::os::unix::fs::symlink(&inc_path, dir.join("latest"));

        let cfg = Config {
            full_backup_day: "Neverday".to_string(),
            incremental_every_n_days: 1,
            ..Config::default()
        };
        assert_eq!(auto_kind(&cfg, &dir, false).unwrap(), BackupKind::Skip);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn auto_kind_incremental_when_period_elapsed() {
        let dir = tmp_dir("auto-inc");
        make_snapshot_dir(&dir, "full-2020-01-01_120000");
        // Last incremental was 5 days ago.
        let old_date = (chrono::Local::now() - chrono::Duration::days(5))
            .format("%Y-%m-%d")
            .to_string();
        make_snapshot_dir(&dir, &format!("inc-{old_date}_120000"));

        let cfg = Config {
            full_backup_day: "Neverday".to_string(),
            incremental_every_n_days: 1,
            ..Config::default()
        };
        assert_eq!(
            auto_kind(&cfg, &dir, false).unwrap(),
            BackupKind::Incremental
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    // ── link_dest_args / full-backup space estimate ───────────────────────────

    #[test]
    fn link_dest_args_incremental_hardlinks_without_checksum() {
        let latest = Path::new("/mnt/backup/inc-2024-01-01_120000");
        let args = link_dest_args(BackupKind::Incremental, Some(latest));
        assert_eq!(
            args,
            ["--link-dest=/mnt/backup/inc-2024-01-01_120000".to_string()]
        );
    }

    #[test]
    fn link_dest_args_full_hardlinks_and_verifies_by_checksum() {
        let latest = Path::new("/mnt/backup/full-2024-01-01_120000");
        let args = link_dest_args(BackupKind::Full, Some(latest));
        assert_eq!(
            args,
            [
                "--link-dest=/mnt/backup/full-2024-01-01_120000".to_string(),
                "--checksum".to_string()
            ]
        );
    }

    #[test]
    fn link_dest_args_empty_when_no_previous_snapshot() {
        assert!(link_dest_args(BackupKind::Full, None).is_empty());
        assert!(link_dest_args(BackupKind::Incremental, None).is_empty());
    }

    #[test]
    fn hardlinked_full_needs_headroom_not_a_second_copy() {
        const GIB: u64 = 1024 * 1024 * 1024;
        // Small volume: the floor applies.
        assert_eq!(hardlinked_full_bytes_needed(100 * GIB), 10 * GIB);
        // 931 GiB drive: ~2% headroom, nowhere near the 700+ GiB the old
        // estimate demanded.
        let needed = hardlinked_full_bytes_needed(931 * GIB);
        assert!(needed > 10 * GIB && needed < 30 * GIB, "needed={needed}");
    }

    #[test]
    fn hardlinked_full_floor_scales_down_for_small_drives() {
        const GIB: u64 = 1024 * 1024 * 1024;
        const MIB: u64 = 1024 * 1024;
        // 8 GiB drive holding a ~500 MiB backup: ~7.5 GiB is free and nothing
        // changed, so the full must be allowed (a fixed 10 GiB floor would
        // defer it forever).
        let total = 8 * GIB;
        let needed = hardlinked_full_bytes_needed(total);
        assert!(needed <= total / 10 + MIB, "needed={needed}");
        assert!(7 * GIB + 512 * MIB >= needed, "free space must satisfy it");
        // Never demands more than the whole drive, however small.
        for total in [64 * MIB, 512 * MIB, GIB, 4 * GIB] {
            assert!(hardlinked_full_bytes_needed(total) < total, "total={total}");
        }
        // The 10 GiB floor still applies from 100 GiB up.
        assert_eq!(hardlinked_full_bytes_needed(100 * GIB), 10 * GIB);
        assert_eq!(hardlinked_full_bytes_needed(500 * GIB), 10 * GIB);
    }

    #[test]
    fn full_backup_bytes_needed_uses_floor_when_no_snapshot_exists() {
        let dir = tmp_dir("full-needed-fresh");
        assert_eq!(
            full_backup_bytes_needed(&dir, 931 * 1024 * 1024 * 1024),
            FRESH_FULL_MIN_BYTES
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn full_backup_bytes_needed_is_small_when_a_snapshot_exists() {
        let dir = tmp_dir("full-needed-hardlinked");
        make_snapshot_dir(&dir, "full-2024-01-01_120000");
        let total = 931u64 * 1024 * 1024 * 1024;
        assert_eq!(
            full_backup_bytes_needed(&dir, total),
            hardlinked_full_bytes_needed(total)
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    // ── purge_stale_inprogress / purge_stale_rsync_logs ──────────────────────────────

    fn open_test_log(dir: &Path) -> fs::File {
        fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("test.log"))
            .unwrap()
    }

    #[test]
    fn purge_stale_inprogress_removes_only_staging_dirs() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tmp_dir("purge-inprogress");
        let outside = tmp_dir("purge-inprogress-outside");
        fs::write(outside.join("keep-me.txt"), "x").unwrap();

        // Stale staging dirs, one with a read-only nested dir like a Go module cache.
        make_snapshot_dir(&dir, ".inprogress-full-2024-01-01_120000");
        let nested = dir.join(".inprogress-inc-2024-01-02_120000/go/pkg/mod");
        fs::create_dir_all(&nested).unwrap();
        fs::write(nested.join("go.mod"), "module x\n").unwrap();
        let mut perms = fs::metadata(&nested).unwrap().permissions();
        perms.set_mode(0o555);
        fs::set_permissions(&nested, perms).unwrap();

        // Things that must survive.
        make_snapshot_dir(&dir, "full-2024-01-01_120000");
        make_snapshot_dir(&dir, "inc-2024-01-02_120000");
        make_snapshot_dir(&dir, "not-inprogress");
        fs::write(dir.join(".inprogress-file"), "a file, not a directory").unwrap();
        std::os::unix::fs::symlink(&outside, dir.join(".inprogress-link")).unwrap();

        let mut log = open_test_log(&dir);
        assert_eq!(purge_stale_inprogress(&dir, &mut log), 2);

        assert!(!dir.join(".inprogress-full-2024-01-01_120000").exists());
        assert!(!dir.join(".inprogress-inc-2024-01-02_120000").exists());
        assert!(dir.join("full-2024-01-01_120000").exists());
        assert!(dir.join("inc-2024-01-02_120000").exists());
        assert!(dir.join("not-inprogress").exists());
        assert!(
            dir.join(".inprogress-file").exists(),
            "files are not purged"
        );
        assert!(
            fs::symlink_metadata(dir.join(".inprogress-link")).is_ok(),
            "symlink itself is left alone"
        );
        assert!(
            outside.join("keep-me.txt").exists(),
            "a symlink must never be followed"
        );
        let text = fs::read_to_string(dir.join("test.log")).unwrap();
        assert!(text.contains("Removed stale staging directory .inprogress-full-2024-01-01_120000"));

        fs::remove_dir_all(&dir).unwrap();
        fs::remove_dir_all(&outside).unwrap();
    }

    #[test]
    fn arg_refers_to_dir_matches_the_directory_or_paths_inside_it() {
        let name = ".inprogress-inc-2026-10-10_080250";
        assert!(arg_refers_to_dir(&format!("/mnt/b/verona1/{name}"), name));
        assert!(arg_refers_to_dir(&format!("/mnt/b/verona1/{name}/"), name));
        assert!(arg_refers_to_dir(
            &format!("/mnt/b/verona1/{name}/sub/f"),
            name
        ));
        assert!(arg_refers_to_dir(name, name));
        // A different directory, or only a name prefix/suffix, must not match.
        assert!(!arg_refers_to_dir(
            "/mnt/b/verona1/.inprogress-inc-2026-10-10_080251",
            name
        ));
        assert!(!arg_refers_to_dir(&format!("/mnt/b/verona1/x{name}"), name));
        assert!(!arg_refers_to_dir(
            &format!("/mnt/b/verona1/{name}-old"),
            name
        ));
        assert!(!arg_refers_to_dir("--archive", name));
    }

    #[test]
    fn purge_stale_inprogress_keeps_a_directory_a_live_process_is_using() {
        // Stand-in for an orphaned rsync: a live process whose command line
        // carries the staging directory, as rsync's destination argument does.
        let dir = tmp_dir("purge-inprogress-live");
        let staging = dir.join(".inprogress-inc-2024-01-01_120000");
        let abandoned = dir.join(".inprogress-full-2024-01-02_120000");
        fs::create_dir_all(staging.join("data")).unwrap();
        fs::create_dir_all(&abandoned).unwrap();

        // The trailing `; :` stops sh from exec'ing `sleep` directly, which
        // would drop the staging-directory argument from the command line.
        let mut child = std::process::Command::new("sh")
            .args(["-c", "sleep 30; :", "sh"])
            .arg(&staging)
            .spawn()
            .unwrap();

        let mut log = open_test_log(&dir);
        let removed = purge_stale_inprogress(&dir, &mut log);
        let text = fs::read_to_string(dir.join("test.log")).unwrap();

        assert_eq!(removed, 1, "only the unused directory is purged");
        assert!(
            staging.exists(),
            "directory in use by a live process must be kept"
        );
        assert!(!abandoned.exists());
        assert!(text.contains("Keeping staging directory .inprogress-inc-2024-01-01_120000"));

        // Once that process is gone the directory is stale and is purged.
        child.kill().unwrap();
        child.wait().unwrap();
        assert_eq!(purge_stale_inprogress(&dir, &mut log), 1);
        assert!(!staging.exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn purge_stale_inprogress_missing_dir_is_a_no_op() {
        let dir = tmp_dir("purge-inprogress-missing");
        let mut log = open_test_log(&dir);
        assert_eq!(
            purge_stale_inprogress(&dir.join("does-not-exist"), &mut log),
            0
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn purge_stale_rsync_logs_removes_only_old_rsync_logs() {
        let dir = tmp_dir("purge-rsync-logs");
        let old = dir.join("rsync-2020-01-01_120000.log");
        let fresh = dir.join("rsync-current.log");
        fs::write(&old, "old").unwrap();
        fs::write(&fresh, "fresh").unwrap();
        std::process::Command::new("touch")
            .args(["-t", "202001010000", old.to_str().unwrap()])
            .status()
            .unwrap();
        // Same age as `old` but not an rsync log: must survive.
        let unrelated = dir.join("backup.log");
        let wrong_ext = dir.join("rsync-notes.txt");
        for p in [&unrelated, &wrong_ext] {
            fs::write(p, "keep").unwrap();
            std::process::Command::new("touch")
                .args(["-t", "202001010000", p.to_str().unwrap()])
                .status()
                .unwrap();
        }

        let mut log = open_test_log(&dir);
        assert_eq!(
            purge_stale_rsync_logs(&dir, Duration::from_secs(24 * 3600), &mut log),
            1
        );
        assert!(!old.exists(), "old rsync log should be purged");
        assert!(fresh.exists(), "a fresh rsync log must be kept");
        assert!(unrelated.exists());
        assert!(wrong_ext.exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    // ── apply_retention ───────────────────────────────────────────────────────

    #[test]
    fn apply_retention_never_removes_full_snapshots() {
        let dir = tmp_dir("retention-full");
        let full = dir.join("full-2020-01-01_120000");
        fs::create_dir_all(&full).unwrap();
        // Age the directory so it would be pruned if retention checked it.
        std::process::Command::new("touch")
            .args(["-t", "202001010000", full.to_str().unwrap()])
            .status()
            .unwrap();

        let log_path = dir.join("test.log");
        let mut log = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .unwrap();
        apply_retention(&dir, 0, &mut log).unwrap(); // retention_days = 0 → prune all old
        assert!(
            full.exists(),
            "full-* snapshot must never be pruned by retention"
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn apply_retention_removes_old_incrementals() {
        let dir = tmp_dir("retention-inc");
        let inc = dir.join("inc-2020-01-01_120000");
        fs::create_dir_all(&inc).unwrap();
        // Backdate the directory to 2020 so retention (0 days) will prune it.
        std::process::Command::new("touch")
            .args(["-t", "202001010000", inc.to_str().unwrap()])
            .status()
            .unwrap();

        let log_path = dir.join("test.log");
        let mut log = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .unwrap();
        apply_retention(&dir, 0, &mut log).unwrap();
        assert!(!inc.exists(), "old inc-* snapshot should be pruned");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn apply_retention_keeps_recent_incrementals() {
        let dir = tmp_dir("retention-keep");
        let inc = dir.join("inc-2099-01-01_120000");
        fs::create_dir_all(&inc).unwrap(); // mtime = now → within any sane retention window

        let log_path = dir.join("test.log");
        let mut log = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .unwrap();
        apply_retention(&dir, 30, &mut log).unwrap();
        assert!(inc.exists(), "recent inc-* snapshot must not be pruned");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn prune_all_incrementals_removes_all_incs_keeps_fulls() {
        let dir = tmp_dir("prune-all-inc");
        make_snapshot_dir(&dir, "full-2024-01-01_120000");
        make_snapshot_dir(&dir, "inc-2024-01-02_120000");
        make_snapshot_dir(&dir, "inc-2024-01-03_120000");

        let log_path = dir.join("test.log");
        let mut log = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .unwrap();
        prune_all_incrementals(&dir, &mut log).unwrap();

        assert!(dir.join("full-2024-01-01_120000").exists());
        assert!(!dir.join("inc-2024-01-02_120000").exists());
        assert!(!dir.join("inc-2024-01-03_120000").exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn try_remove_snapshot_deletes_readonly_go_module_dirs() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tmp_dir("readonly-gomod");
        let snap = dir.join("inc-2024-01-02_120000");
        let nested = snap.join("go/pkg/mod/example.com/mod@v1.0.0");
        fs::create_dir_all(&nested).unwrap();
        fs::write(nested.join("go.mod"), "module example\n").unwrap();
        let mut perms = fs::metadata(&nested).unwrap().permissions();
        perms.set_mode(0o555);
        fs::set_permissions(&nested, perms).unwrap();

        let log_path = dir.join("test.log");
        let mut log = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .unwrap();
        try_remove_snapshot(
            &snap,
            "inc-2024-01-02_120000",
            &mut log,
            "incremental snapshot",
        );
        assert!(
            !snap.exists(),
            "0555 Go module dirs must not block snapshot deletion"
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn try_remove_snapshot_deletes_unreadable_dirs() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tmp_dir("unreadable-dir");
        let snap = dir.join("inc-2024-01-02_120000");
        let nested = snap.join("secret/locked");
        fs::create_dir_all(&nested).unwrap();
        fs::write(nested.join("x"), "x").unwrap();
        let mut perms = fs::metadata(&nested).unwrap().permissions();
        perms.set_mode(0o000);
        fs::set_permissions(&nested, perms).unwrap();

        let log_path = dir.join("test.log");
        let mut log = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .unwrap();
        try_remove_snapshot(
            &snap,
            "inc-2024-01-02_120000",
            &mut log,
            "incremental snapshot",
        );
        assert!(!snap.exists(), "0000 dirs must not block snapshot deletion");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn apply_full_retention_keeps_n_newest_fulls() {
        let dir = tmp_dir("full-retention");
        make_snapshot_dir(&dir, "full-2024-01-01_120000");
        make_snapshot_dir(&dir, "full-2024-02-01_120000");
        make_snapshot_dir(&dir, "full-2024-03-01_120000");

        let log_path = dir.join("test.log");
        let mut log = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .unwrap();
        apply_full_retention(&dir, 2, &mut log).unwrap();

        assert!(!dir.join("full-2024-01-01_120000").exists());
        assert!(dir.join("full-2024-02-01_120000").exists());
        assert!(dir.join("full-2024-03-01_120000").exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn apply_full_retention_zero_means_unlimited() {
        let dir = tmp_dir("full-retention-unlimited");
        make_snapshot_dir(&dir, "full-2024-01-01_120000");
        make_snapshot_dir(&dir, "full-2024-02-01_120000");

        let log_path = dir.join("test.log");
        let mut log = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .unwrap();
        apply_full_retention(&dir, 0, &mut log).unwrap();

        assert!(dir.join("full-2024-01-01_120000").exists());
        assert!(dir.join("full-2024-02-01_120000").exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn archive_log_to_snapshot_copies_into_snapshot() {
        let snapshot = tmp_dir("log-archive");
        let active_log = snapshot.join("active.log");
        fs::write(
            &active_log,
            "[2024-01-01] weekly log entry\n2024/01/01 rsync noise\n[2024-01-02] another entry\n",
        )
        .unwrap();

        let archived = archive_log_to_snapshot(&active_log, &snapshot)
            .unwrap()
            .unwrap();
        assert_eq!(archived, snapshot.join(".backup-tool/backup.log"));
        let text = fs::read_to_string(archived).unwrap();
        assert!(text.contains("weekly log entry"));
        assert!(text.contains("another entry"));
        assert!(!text.contains("rsync noise"));
        fs::remove_dir_all(&snapshot).unwrap();
    }

    #[test]
    fn archive_log_to_snapshot_skips_missing_or_empty() {
        let snapshot = tmp_dir("log-archive-empty");
        let missing = snapshot.join("missing.log");
        assert!(archive_log_to_snapshot(&missing, &snapshot)
            .unwrap()
            .is_none());

        fs::write(&missing, "2024/01/01 only rsync lines\n").unwrap();
        assert!(archive_log_to_snapshot(&missing, &snapshot)
            .unwrap()
            .is_none());
        fs::remove_dir_all(&snapshot).unwrap();
    }
}
