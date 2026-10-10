use anyhow::{Context, Result};
use std::{
    path::{Path, PathBuf},
    process::Command,
};

use crate::config::Config;

// ── Constants ─────────────────────────────────────────────────────────────────

const SERVICE: &str = "backup-tool.service";
const TIMER: &str = "backup-tool.timer";
/// Pre-0.1.7 name of the service; installs may still hold a dangling link to it.
const LEGACY_SERVICE: &str = "home-backup.service";
const APP_ID: &str = "backup-tool";

// ── Paths ─────────────────────────────────────────────────────────────────────

fn systemd_user_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from(".config"))
        .join("systemd")
        .join("user")
}

fn applications_dir() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from(".local/share"))
        .join("applications")
}

fn icon_dir_128() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from(".local/share"))
        .join("icons")
        .join("hicolor")
        .join("128x128")
        .join("apps")
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Install the systemd service + timer units and the .desktop / icon files.
///
/// Returns a multi-line human-readable install log for display in the recap.
pub fn install(config: &Config) -> Result<String> {
    let mut log = Vec::<String>::new();

    let bin = current_exe()?;
    let (h, m) = config.backup_hm();
    let sd_dir = systemd_user_dir();
    std::fs::create_dir_all(&sd_dir).with_context(|| format!("creating {}", sd_dir.display()))?;

    write_service_unit(&sd_dir, &bin)?;
    log.push(format!("  Wrote {}", sd_dir.join(SERVICE).display()));

    write_timer_unit(&sd_dir, h, m)?;
    log.push(format!("  Wrote {}", sd_dir.join(TIMER).display()));

    for name in remove_boot_hook(&sd_dir) {
        log.push(format!(
            "  Removed stale default.target link for {name} (only the timer starts backups)"
        ));
    }

    // ── Reload & enable ───────────────────────────────────────────────────
    run_systemctl(&["daemon-reload"])?;
    log.push("  systemctl --user daemon-reload".to_string());

    run_systemctl(&["enable", "--now", TIMER])?;
    log.push(format!("  Enabled and started {TIMER}"));

    // ── Desktop icon & launcher ────────────────────────────────────────────────
    install_desktop_files(&bin, &mut log)?;

    // ── Nautilus bookmark ───────────────────────────────────────────────
    match manage_nautilus_bookmark(&config.dest_dir) {
        Ok(()) => log.push(format!("  Updated Nautilus bookmark → {}", config.dest_dir)),
        Err(e) => log.push(format!("  ⚠  Nautilus bookmark skipped: {e}")),
    }

    Ok(log.join("\n"))
}

/// Regenerate and reload the timer unit after a schedule change.
pub fn update_timer(config: &Config) -> Result<()> {
    let bin = current_exe()?;
    let (h, m) = config.backup_hm();
    let sd_dir = systemd_user_dir();
    std::fs::create_dir_all(&sd_dir).with_context(|| format!("creating {}", sd_dir.display()))?;

    write_service_unit(&sd_dir, &bin)?;
    write_timer_unit(&sd_dir, h, m)?;
    remove_boot_hook(&sd_dir);
    run_systemctl(&["daemon-reload"])?;
    run_systemctl(&["enable", "--now", TIMER])?;
    Ok(())
}

/// Disable and stop the timer.
pub fn disable() -> Result<()> {
    run_systemctl(&["disable", "--now", TIMER])?;
    Ok(())
}

/// Is the timer currently active?
pub fn timer_is_active() -> bool {
    Command::new("systemctl")
        .args(["--user", "is-active", "--quiet", TIMER])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Human-readable one-liner: "Next: <date>  |  Last: <date>".
pub fn timer_status_line() -> String {
    let out = Command::new("systemctl")
        .args(["--user", "list-timers", TIMER, "--no-pager", "--no-legend"])
        .output();

    match out {
        Ok(o) if o.status.success() => {
            let s = String::from_utf8_lossy(&o.stdout);
            // Columns: NEXT, LEFT, LAST, PASSED, UNIT, ACTIVATES
            // We want columns 0 (NEXT timestamp) and 2 (LAST timestamp).
            if let Some(line) = s.lines().next() {
                let cols: Vec<&str> = line.splitn(7, ' ').filter(|s| !s.is_empty()).collect();
                let next = cols.first().copied().unwrap_or("—");
                let last = cols.get(2).copied().unwrap_or("—");
                return format!("Next: {next}   Last: {last}");
            }
            "Timer is installed but has not run yet".to_string()
        }
        _ => "Timer status unavailable".to_string(),
    }
}

// ── Internal helpers ──────────────────────────────────────────────────────────

/// Remove links that make `backup-tool.service` start whenever the user
/// manager starts (every boot with linger, every login).
///
/// The service is meant to be started only by `backup-tool.timer`. Enabling the
/// service itself (`systemctl --user enable backup-tool.service`, or the old
/// `[Install] WantedBy=default.target` this tool used to write) ran a backup
/// about 20 seconds after boot, before any session exists, and duplicated the
/// timer's own `Persistent=true` catch-up. Also removes a *dangling* link to
/// the pre-rename `home-backup.service`. Only symlinks are removed; returns the
/// unit names whose link was removed.
fn remove_boot_hook(sd_dir: &Path) -> Vec<String> {
    let wants = sd_dir.join("default.target.wants");
    let mut removed = Vec::new();
    for (name, only_if_dangling) in [(SERVICE, false), (LEGACY_SERVICE, true)] {
        let link = wants.join(name);
        let Ok(meta) = std::fs::symlink_metadata(&link) else {
            continue;
        };
        if !meta.file_type().is_symlink() {
            continue;
        }
        // exists() follows the link, so it is false only for a dangling one.
        if only_if_dangling && link.exists() {
            continue;
        }
        if std::fs::remove_file(&link).is_ok() {
            removed.push(name.to_string());
        }
    }
    removed
}

/// The service has deliberately no `[Install]` section: only the timer starts it.
fn write_service_unit(sd_dir: &Path, bin: &str) -> Result<()> {
    let svc = format!(
        "[Unit]\n\
         Description=Backup-Tool automatic backup\n\
         Documentation=file://{data_dir}/backup.log\n\
         After=default.target\n\
         \n\
         [Service]\n\
         Type=oneshot\n\
         ExecStart={bin} backup auto\n\
         SyslogIdentifier={app}\n\
         StandardOutput=journal\n\
         StandardError=journal\n\
         TimeoutStartSec=infinity\n\
         TimeoutStopSec=3600\n",
        data_dir = Config::data_dir().display(),
        bin = bin,
        app = APP_ID,
    );
    let svc_path = sd_dir.join(SERVICE);
    std::fs::write(&svc_path, &svc).with_context(|| format!("writing {}", svc_path.display()))?;
    Ok(())
}

fn write_timer_unit(sd_dir: &Path, h: u8, m: u8) -> Result<()> {
    let timer = format!(
        "[Unit]\n\
         Description=Daily Backup-Tool Timer\n\
         \n\
         [Timer]\n\
         OnCalendar=*-*-* {h:02}:{m:02}:00\n\
         AccuracySec=1min\n\
         RandomizedDelaySec=300\n\
         Persistent=true\n\
         \n\
         [Install]\n\
         WantedBy=timers.target\n",
    );
    let timer_path = sd_dir.join(TIMER);
    std::fs::write(&timer_path, &timer)
        .with_context(|| format!("writing {}", timer_path.display()))?;
    Ok(())
}

fn run_systemctl(args: &[&str]) -> Result<()> {
    let mut full_args = vec!["--user"];
    full_args.extend_from_slice(args);
    let status = Command::new("systemctl")
        .args(&full_args)
        .status()
        .with_context(|| format!("running systemctl --user {}", args.join(" ")))?;
    if !status.success() {
        anyhow::bail!("systemctl --user {} returned non-zero", args.join(" "));
    }
    Ok(())
}

fn current_exe() -> Result<String> {
    std::env::current_exe()
        .context("resolving current executable path")
        .map(|p| p.to_string_lossy().into_owned())
}

/// Add (or refresh) a Nautilus/Files sidebar bookmark for the backup destination.
///
/// Strips any pre-existing `backup-tool` entries first so re-installs don't
/// accumulate stale bookmarks.
fn manage_nautilus_bookmark(dest_dir: &str) -> Result<()> {
    let bookmarks_path = dirs::config_dir()
        .context("no config dir")?
        .join("gtk-3.0")
        .join("bookmarks");

    // Ensure the parent directory exists (GTK3 may not have created it yet).
    if let Some(parent) = bookmarks_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // Read the existing bookmark list (empty string if the file doesn't exist).
    let existing = if bookmarks_path.exists() {
        std::fs::read_to_string(&bookmarks_path)?
    } else {
        String::new()
    };

    // Remove any lines that were previously added by this tool:
    // match on the URI prefix so both old and new dest paths are cleaned.
    let filtered: Vec<&str> = existing
        .lines()
        .filter(|l| {
            // Drop lines that look like a previously installed backup bookmark.
            !l.contains("home_backups") && !l.contains("backup-tool")
        })
        .collect();

    // Append the fresh entry.  The label shown in the sidebar follows the URI.
    let dest_uri = format!("file://{dest_dir}");
    let new_line = format!("{dest_uri} Backup-Tool");

    let mut output = filtered.join("\n");
    if !output.is_empty() && !output.ends_with('\n') {
        output.push('\n');
    }
    output.push_str(&new_line);
    output.push('\n');

    std::fs::write(&bookmarks_path, output)?;
    Ok(())
}

fn install_desktop_files(bin: &str, log: &mut Vec<String>) -> Result<()> {
    // ── Icon ──────────────────────────────────────────────────────────────
    let icon_dir = icon_dir_128();
    std::fs::create_dir_all(&icon_dir)
        .with_context(|| format!("creating icon dir {}", icon_dir.display()))?;

    // Copy the bundled icon from assets next to the binary.
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("/usr/local/bin"));

    let icon_src = exe_dir.join("assets").join("backup-tool.png");
    let icon_dest = icon_dir.join("backup-tool.png");

    if icon_src.exists() {
        std::fs::copy(&icon_src, &icon_dest)
            .with_context(|| format!("copying icon to {}", icon_dest.display()))?;
        log.push(format!("  Installed icon → {}", icon_dest.display()));

        // Refresh icon cache (best-effort).
        let _ = Command::new("gtk-update-icon-cache")
            .arg("-f")
            .arg("-t")
            .arg(
                icon_dir
                    .parent()
                    .and_then(|p| p.parent())
                    .and_then(|p| p.parent())
                    .unwrap_or(&icon_dir),
            )
            .status();
    } else {
        log.push("  ⚠  Icon asset not found next to binary — skipping icon install".to_string());
    }

    // ── .desktop file ─────────────────────────────────────────────────────
    let apps_dir = applications_dir();
    std::fs::create_dir_all(&apps_dir)
        .with_context(|| format!("creating applications dir {}", apps_dir.display()))?;

    let desktop_content = format!(
        "[Desktop Entry]\n\
         Name=Backup-Tool\n\
         Comment=Backup-Tool — rsync backups to an external drive\n\
         Exec={bin}\n\
         Icon=backup-tool\n\
         Terminal=false\n\
         Type=Application\n\
         Categories=System;Utility;\n\
         Keywords=backup;rsync;snapshot;\n\
         StartupNotify=true\n",
    );
    let desktop_path = apps_dir.join("backup-tool.desktop");
    std::fs::write(&desktop_path, &desktop_content)
        .with_context(|| format!("writing {}", desktop_path.display()))?;
    log.push(format!("  Installed launcher → {}", desktop_path.display()));

    // Update desktop database (best-effort).
    let _ = Command::new("update-desktop-database")
        .arg(&apps_dir)
        .status();

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn service_unit_uses_backup_subcommand_not_legacy_flag() {
        let dir =
            std::env::temp_dir().join(format!("backup-tool-systemd-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        write_service_unit(&dir, "/usr/bin/backup-tool").unwrap();
        let svc = fs::read_to_string(dir.join(SERVICE)).unwrap();
        assert!(
            svc.contains("ExecStart=/usr/bin/backup-tool backup auto"),
            "unexpected service unit:\n{svc}"
        );
        assert!(
            !svc.contains("--backup"),
            "legacy --backup flag must not appear in generated unit"
        );
        assert!(
            svc.contains("Description=Backup-Tool automatic backup"),
            "unexpected service Description:\n{svc}"
        );

        let _ = fs::remove_dir_all(dir);
    }

    fn unique_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("backup-tool-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn service_unit_has_no_install_section_so_only_the_timer_starts_it() {
        let dir = unique_dir("no-install");
        write_service_unit(&dir, "/usr/bin/backup-tool").unwrap();
        let svc = fs::read_to_string(dir.join(SERVICE)).unwrap();
        assert!(!svc.contains("[Install]"), "service unit:\n{svc}");
        assert!(!svc.contains("WantedBy"), "service unit:\n{svc}");
        assert!(svc.contains("Type=oneshot"));

        // The timer keeps its [Install] so `enable` of the timer still works.
        write_timer_unit(&dir, 2, 0).unwrap();
        let timer = fs::read_to_string(dir.join(TIMER)).unwrap();
        assert!(
            timer.contains("WantedBy=timers.target"),
            "timer unit:\n{timer}"
        );
        assert!(timer.contains("Persistent=true"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn remove_boot_hook_removes_service_link_and_dangling_legacy_link_only() {
        use std::os::unix::fs::symlink;

        let dir = unique_dir("boot-hook");
        let wants = dir.join("default.target.wants");
        fs::create_dir_all(&wants).unwrap();
        // Real unit files the links point at.
        fs::write(dir.join(SERVICE), "unit").unwrap();
        fs::write(dir.join("other.service"), "unit").unwrap();
        // The stray boot hook, a dangling legacy link, and an unrelated link.
        symlink(dir.join(SERVICE), wants.join(SERVICE)).unwrap();
        symlink(dir.join(LEGACY_SERVICE), wants.join(LEGACY_SERVICE)).unwrap(); // target missing
        symlink(dir.join("other.service"), wants.join("other.service")).unwrap();

        let mut removed = remove_boot_hook(&dir);
        removed.sort();
        assert_eq!(removed, [SERVICE, LEGACY_SERVICE]); // sorted by name
        assert!(fs::symlink_metadata(wants.join(SERVICE)).is_err());
        assert!(fs::symlink_metadata(wants.join(LEGACY_SERVICE)).is_err());
        assert!(
            fs::symlink_metadata(wants.join("other.service")).is_ok(),
            "unrelated links must be left alone"
        );
        assert!(dir.join(SERVICE).exists(), "the unit file itself is kept");

        // Idempotent.
        assert!(remove_boot_hook(&dir).is_empty());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn remove_boot_hook_keeps_a_legacy_link_whose_target_still_exists() {
        use std::os::unix::fs::symlink;

        let dir = unique_dir("boot-hook-live-legacy");
        let wants = dir.join("default.target.wants");
        fs::create_dir_all(&wants).unwrap();
        fs::write(dir.join(LEGACY_SERVICE), "unit").unwrap();
        symlink(dir.join(LEGACY_SERVICE), wants.join(LEGACY_SERVICE)).unwrap();

        assert!(remove_boot_hook(&dir).is_empty());
        assert!(fs::symlink_metadata(wants.join(LEGACY_SERVICE)).is_ok());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn remove_boot_hook_ignores_a_regular_file_and_a_missing_directory() {
        let dir = unique_dir("boot-hook-edge");
        assert!(remove_boot_hook(&dir).is_empty(), "no wants dir at all");

        let wants = dir.join("default.target.wants");
        fs::create_dir_all(&wants).unwrap();
        fs::write(wants.join(SERVICE), "not a symlink").unwrap();
        assert!(remove_boot_hook(&dir).is_empty());
        assert!(
            wants.join(SERVICE).exists(),
            "regular files are never deleted"
        );
        let _ = fs::remove_dir_all(dir);
    }
}
