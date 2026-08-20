use anyhow::{Context, Result};
use std::{
    path::{Path, PathBuf},
    process::Command,
};

use crate::config::Config;

// ── Constants ─────────────────────────────────────────────────────────────────

const SERVICE: &str = "backup-tool.service";
const TIMER: &str = "backup-tool.timer";
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

fn write_service_unit(sd_dir: &Path, bin: &str) -> Result<()> {
    let svc = format!(
        "[Unit]\n\
         Description=Home Directory Backup\n\
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
         TimeoutStopSec=3600\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
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
    let new_line = format!("{dest_uri} Home Backups");

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
         Comment=Manage home-directory rsync backups\n\
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
