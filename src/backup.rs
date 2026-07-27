use anyhow::{bail, Context, Result};
use chrono::Local;
use std::{
    fs,
    io::Write,
    os::unix::fs as unix_fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use crate::{config::Config, drives};

/// Which kind of snapshot to create.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackupKind {
    /// A fresh snapshot — no `--link-dest`, every file is copied.
    Full,
    /// Uses `--link-dest` pointing at the latest snapshot, hardlinking
    /// unchanged files so only deltas consume additional space.
    Incremental,
    /// Decide automatically: full on the configured day-of-week or when no
    /// full snapshot exists yet; incremental otherwise.
    Auto,
    /// The incremental period has not elapsed since the last snapshot;
    /// exit cleanly without creating a new snapshot.
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

/// Run a backup, appending progress to the application log.
///
/// Returns a human-readable summary suitable for display in the GUI.
pub fn run(config: &Config, kind: BackupKind) -> Result<String> {
    // ── 1. Ensure the destination directory is reachable ──────────────────
    let dest_root = resolve_dest(config)?;

    // ── 2. Determine full vs incremental ──────────────────────────────────
    let effective_kind = match kind {
        BackupKind::Auto => auto_kind(config, &dest_root),
        BackupKind::Skip => {
            return Ok("Backup skipped (called with Skip kind directly).".to_string());
        }
        other => other,
    };
    if effective_kind == BackupKind::Skip {
        let msg = format!(
            "Backup skipped — last snapshot is within the {}-day incremental period.",
            config.incremental_every_n_days
        );
        return Ok(msg);
    }

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
    if let Some(p) = log_path.parent() {
        fs::create_dir_all(p)?;
    }
    let mut log_file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
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
    cmd.arg(format!("--log-file={}", log_path.display()));

    // For incrementals: hardlink unchanged files from the latest snapshot.
    if effective_kind == BackupKind::Incremental {
        if let Some(link) = resolve_latest(&dest_root) {
            cmd.arg(format!("--link-dest={}", link.display()));
        }
    }

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
        let _ = fs::remove_dir_all(&temp_dir);
        bail!("rsync failed with exit code {exit_code}");
    }

    if exit_code == 24 {
        writeln!(
            log_file,
            "[{log_ts}] WARNING: exit 24 — some files vanished during transfer (non-fatal)"
        )?;
    }

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
        "[{log_ts}] Completed {prefix} snapshot: {}",
        final_dir.file_name().unwrap_or_default().to_string_lossy()
    )?;

    // ── 9. Retention: prune old incrementals ──────────────────────────────
    apply_retention(&dest_root, config.retention_days, &mut log_file)?;

    // ── 10. Desktop notification (best-effort) ─────────────────────────────
    let _ = notify(
        "Home Backup",
        &format!("{prefix} snapshot created successfully"),
    );

    // Build summary string for the GUI.
    let summary = build_summary(prefix, &final_dir, &stdout);
    Ok(summary)
}

// ── Helpers ──────────────────────────────────────────────────────────────────

/// Ensure the backup destination is accessible, trying auto-mount when
/// needed.  Returns the resolved `PathBuf`.
fn resolve_dest(config: &Config) -> Result<PathBuf> {
    let dest = Path::new(&config.dest_dir);
    if dest.exists() {
        return Ok(dest.to_path_buf());
    }

    // Try mounting by UUID.
    if let Some(uuid) = &config.drive_uuid {
        // Already mounted?
        if let Some(mp) = drives::find_mountpoint_by_uuid(uuid) {
            // dest might be a subdirectory on the drive.
            let candidate = PathBuf::from(&mp).join(dest.strip_prefix("/").unwrap_or(dest));
            if candidate.exists() {
                return Ok(dest.to_path_buf());
            }
            // Mount succeeded but subdir missing — create it.
            fs::create_dir_all(dest)
                .with_context(|| format!("creating dest dir {}", dest.display()))?;
            return Ok(dest.to_path_buf());
        }

        let mp =
            drives::mount_by_uuid(uuid).with_context(|| format!("auto-mounting UUID {uuid}"))?;

        let candidate = PathBuf::from(&mp);
        if !dest.starts_with(&candidate) {
            // dest_dir is an absolute path on the drive; create it.
            fs::create_dir_all(dest).with_context(|| format!("creating dest dir after mount"))?;
        }

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

/// Decide the backup kind based on the day of week, existing snapshots,
/// and the configured incremental period.
fn auto_kind(config: &Config, dest_root: &Path) -> BackupKind {
    let today = Local::now().format("%A").to_string(); // "Monday", "Tuesday", …
    let is_full_day = today.eq_ignore_ascii_case(&config.full_backup_day);

    let full_exists = dest_root
        .read_dir()
        .map(|rd| {
            rd.flatten()
                .any(|e| e.file_name().to_string_lossy().starts_with("full-"))
        })
        .unwrap_or(false);

    if is_full_day || !full_exists {
        return BackupKind::Full;
    }

    // Check whether the incremental period has elapsed since the last snapshot.
    // Snapshot names: {full,inc}-YYYY-MM-DD_HHmmss
    let period = config.incremental_every_n_days.max(1);
    if period > 1 {
        if let Some(latest) = resolve_latest(dest_root) {
            let name = latest.file_name().unwrap_or_default().to_string_lossy().into_owned();
            // Date starts at index 5 (after "full-" or "inc-")
            if let Some(date_str) = name.get(5..15) {
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
    entries.sort_by(|a, b| b.file_name().cmp(&a.file_name()));
    entries.first().map(|e| e.path())
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
            if let Err(e) = fs::remove_dir_all(entry.path()) {
                let ts = Local::now().format("%Y-%m-%d %H:%M:%S");
                let _ = writeln!(
                    log,
                    "[{ts}] WARNING: could not remove old snapshot {n}: {e}"
                );
            } else {
                let ts = Local::now().format("%Y-%m-%d %H:%M:%S");
                let _ = writeln!(log, "[{ts}] Retention: removed {n}");
            }
        }
    }
    Ok(())
}

/// Send a desktop notification (best-effort; silently ignored if no display).
fn notify(title: &str, body: &str) -> Result<()> {
    Command::new("notify-send")
        .args(["-u", "normal", "-t", "5000", title, body])
        .status()?;
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

    format!("✅  {prefix} snapshot complete\nSnapshot: {name}\n\n{stats}")
}
