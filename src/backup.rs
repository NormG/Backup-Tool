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
            "Backup skipped (incremental period {n}d — last snapshot is recent enough).",
            n = config.incremental_every_n_days
        );
        return Ok(msg);
    }

    // ── 2b. Full backup: prune first, then verify there is room ───────────
    let mut deferred_full = false;
    let effective_kind = if effective_kind == BackupKind::Full {
        match prepare_full_backup(config, &dest_root)? {
            FullPrep::Proceed => BackupKind::Full,
            FullPrep::DeferredToIncremental { available, needed } => {
                deferred_full = true;
                let msg = format!(
                    "Full backup deferred ({} free, {} needed) — running incremental instead.",
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
                return Ok(format!(
                    "Full backup deferred: need {} free but only {} available.\n\
                     Old incrementals were pruned; remove old full-* snapshots or free space on the drive.",
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
        "Backup-Tool",
        &format!("{prefix} snapshot created successfully"),
    );

    // Build summary string for the GUI.
    let mut summary = build_summary(prefix, &final_dir, &stdout);
    if deferred_full {
        summary = format!(
            "⚠️  Scheduled full backup deferred (insufficient space after pruning).\n\n{summary}"
        );
    }
    Ok(summary)
}

// ── Helpers ──────────────────────────────────────────────────────────────────

/// Safety margin applied when comparing free space to the latest snapshot size.
const FULL_BACKUP_SPACE_MARGIN: f64 = 1.10;

enum FullPrep {
    Proceed,
    DeferredToIncremental { available: u64, needed: u64 },
    Blocked { available: u64, needed: u64 },
}

/// Before a full backup: prune old incrementals, then ensure the drive has
/// enough free space for a complete copy (old snapshots remain during rsync).
fn prepare_full_backup(config: &Config, dest_root: &Path) -> Result<FullPrep> {
    let log_path = Config::log_path();
    let mut log = open_log_append(&log_path)?;

    let ts = Local::now().format("%Y-%m-%d %H:%M:%S");
    writeln!(
        log,
        "[{ts}] Full backup scheduled — pruning incrementals older than {}d first",
        config.retention_days
    )?;
    apply_retention(dest_root, config.retention_days, &mut log)?;

    let available = drives::available_bytes(dest_root)?;
    let needed = full_backup_bytes_needed(dest_root)?;

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

/// Estimate bytes required for a new full snapshot, based on the latest
/// snapshot size with a safety margin.  When no snapshot exists yet, require
/// at least 1 GiB as a minimal sanity check.
fn full_backup_bytes_needed(dest_root: &Path) -> Result<u64> {
    if let Some(latest) = resolve_latest(dest_root) {
        let bytes = dir_disk_usage_bytes(&latest)?;
        Ok((bytes as f64 * FULL_BACKUP_SPACE_MARGIN).ceil() as u64)
    } else {
        Ok(1024 * 1024 * 1024)
    }
}

fn dir_disk_usage_bytes(path: &Path) -> Result<u64> {
    let out = Command::new("du")
        .args(["-sb", "--"])
        .arg(path)
        .output()
        .with_context(|| format!("running du on {}", path.display()))?;

    if !out.status.success() {
        bail!(
            "du failed for {}: {}",
            path.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }

    let field = String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .next()
        .context("parsing du output")?
        .to_string();

    field
        .parse::<u64>()
        .with_context(|| format!("parsing du bytes '{field}'"))
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
            fs::create_dir_all(dest).with_context(|| "creating dest dir after mount")?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    // ── helpers ───────────────────────────────────────────────────────────────

    /// Create a unique temp directory for a test and return its path.
    /// The caller is responsible for removing it afterwards.
    fn tmp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("backup-tool-test-{tag}-{}", std::process::id()));
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
    fn auto_kind_full_when_no_full_snapshot_exists() {
        let dir = tmp_dir("auto-no-full");
        // Only inc snapshots present — no full-* directory.
        make_snapshot_dir(&dir, "inc-2024-01-01_120000");

        let cfg = Config {
            full_backup_day: "Neverday".to_string(), // won't match any real weekday
            ..Config::default()
        };
        assert_eq!(auto_kind(&cfg, &dir), BackupKind::Full);
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
        assert_eq!(auto_kind(&cfg, &dir), BackupKind::Skip);
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
        assert_eq!(auto_kind(&cfg, &dir), BackupKind::Incremental);
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
        assert!(full.exists(), "full-* snapshot must never be pruned by retention");
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
