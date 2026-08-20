//! New-year reminder to archive the previous calendar year's backups.

use anyhow::{Context, Result};
use chrono::{Datelike, Local, NaiveDate};
use gtk4::{prelude::*, ApplicationWindow, MessageDialog, MessageType, ResponseType};
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    process::Command,
};

use crate::{config::Config, drives};

/// Persisted dismiss / remind state for the year-end archive prompt.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct YearEndReminderState {
    /// Archive year the user marked complete (e.g. 2025 when dismissed in Jan 2026).
    #[serde(default)]
    pub dismissed_archive_year: Option<u32>,
    /// Last calendar date (`YYYY-MM-DD`) the dialog was shown or snoozed.
    #[serde(default)]
    pub last_reminded_date: Option<String>,
}

/// Snapshot usage attributed to one calendar year on the backup drive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PriorYearUsage {
    pub archive_year: u32,
    pub full_count: u32,
    pub inc_count: u32,
    pub snapshot_bytes: u64,
    pub drive_available: Option<u64>,
    pub drive_total: Option<u64>,
}

impl YearEndReminderState {
    pub fn state_path() -> PathBuf {
        Config::data_dir().join("year-end-reminder.toml")
    }

    pub fn load() -> Result<Self> {
        let path = Self::state_path();
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        toml::from_str(&raw).with_context(|| "parsing year-end-reminder.toml")
    }

    pub fn save(&self) -> Result<()> {
        let path = Self::state_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let raw = toml::to_string_pretty(self).context("serialising year-end reminder")?;
        std::fs::write(&path, raw).with_context(|| format!("writing {}", path.display()))
    }

    /// Returns the archive year to prompt about, if the dialog should open today.
    pub fn should_show_today(&self, today: NaiveDate) -> Option<u32> {
        if today.month() != 1 {
            return None;
        }
        let archive_year = u32::try_from(today.year() - 1).ok()?;
        if self.dismissed_archive_year == Some(archive_year) {
            return None;
        }
        let today_str = today.format("%Y-%m-%d").to_string();
        if self.last_reminded_date.as_deref() == Some(today_str.as_str()) {
            return None;
        }
        Some(archive_year)
    }

    pub fn mark_remind_today(&mut self, today: NaiveDate) {
        self.last_reminded_date = Some(today.format("%Y-%m-%d").to_string());
    }

    pub fn mark_dismissed(&mut self, archive_year: u32) {
        self.dismissed_archive_year = Some(archive_year);
        self.last_reminded_date = None;
    }
}

pub fn today_local() -> NaiveDate {
    Local::now().date_naive()
}

pub fn should_show_year_end_reminder() -> Result<Option<u32>> {
    if force_year_end_reminder() {
        let today = today_local();
        return Ok(u32::try_from(today.year() - 1).ok());
    }
    let state = YearEndReminderState::load()?;
    Ok(state.should_show_today(today_local()))
}

fn force_year_end_reminder() -> bool {
    std::env::var_os("BACKUP_TOOL_FORCE_YEAR_END").is_some()
}

pub fn prior_year_usage(config: &Config, archive_year: u32) -> PriorYearUsage {
    let dest = Path::new(&config.dest_dir);
    let mut usage = PriorYearUsage {
        archive_year,
        full_count: 0,
        inc_count: 0,
        snapshot_bytes: 0,
        drive_available: None,
        drive_total: None,
    };

    if dest.exists() {
        if let Ok((avail, total)) = drives::filesystem_bytes(dest) {
            usage.drive_available = Some(avail);
            usage.drive_total = Some(total);
        }
        if let Ok(summary) = measure_year_snapshots(dest, i32::try_from(archive_year).unwrap_or(0))
        {
            usage.full_count = summary.full_count;
            usage.inc_count = summary.inc_count;
            usage.snapshot_bytes = summary.bytes;
        }
    }

    usage
}

struct YearSnapshotSummary {
    full_count: u32,
    inc_count: u32,
    bytes: u64,
}

fn measure_year_snapshots(dest_root: &Path, year: i32) -> Result<YearSnapshotSummary> {
    let mut summary = YearSnapshotSummary {
        full_count: 0,
        inc_count: 0,
        bytes: 0,
    };

    let entries = match dest_root.read_dir() {
        Ok(rd) => rd,
        Err(_) => return Ok(summary),
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let n = name.to_string_lossy();
        let Some(snapshot_year) = snapshot_year_from_name(&n) else {
            continue;
        };
        if snapshot_year != year {
            continue;
        }
        if n.starts_with("full-") {
            summary.full_count += 1;
        } else if n.starts_with("inc-") {
            summary.inc_count += 1;
        } else {
            continue;
        }
        summary.bytes = summary
            .bytes
            .saturating_add(dir_disk_usage_bytes(&path).unwrap_or(0));
    }

    Ok(summary)
}

fn snapshot_year_from_name(name: &str) -> Option<i32> {
    let prefix_end = name.find('-')?;
    let date_start = prefix_end + 1;
    let year_str = name.get(date_start..date_start + 4)?;
    year_str.parse().ok()
}

fn dir_disk_usage_bytes(path: &Path) -> Result<u64> {
    let out = Command::new("du")
        .args(["-sb", "--"])
        .arg(path)
        .output()
        .with_context(|| format!("running du on {}", path.display()))?;

    if !out.status.success() {
        anyhow::bail!(
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

pub fn format_bytes(n: u64) -> String {
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

fn build_dialog_body(config: &Config, usage: &PriorYearUsage) -> String {
    let dest = &config.dest_dir;
    let snapshot_line = if usage.full_count == 0 && usage.inc_count == 0 {
        format!(
            "No full or incremental snapshots from {} were found under:\n  {dest}",
            usage.archive_year
        )
    } else {
        format!(
            "{} backups on disk: {} full and {} incremental snapshots, about {} total.",
            usage.archive_year,
            usage.full_count,
            usage.inc_count,
            format_bytes(usage.snapshot_bytes)
        )
    };

    let drive_line = match (usage.drive_available, usage.drive_total) {
        (Some(avail), Some(total)) => format!(
            "Backup drive space: {} free of {} total.",
            format_bytes(avail),
            format_bytes(total)
        ),
        _ => format!(
            "Backup destination is not reachable right now:\n  {dest}\n\
             Plug in the backup drive to review space usage."
        ),
    };

    format!(
        "It is January — time to review last year's backups.\n\n\
         {snapshot_line}\n\
         {drive_line}\n\n\
         Consider archiving {year} snapshots to cold storage or moving backups \
         to a new disk before continuing this year.",
        year = usage.archive_year
    )
}

/// Show the year-end archive reminder if appropriate for today.
pub fn maybe_show_year_end_reminder(parent: &ApplicationWindow, config: &Config) {
    let Ok(Some(archive_year)) = should_show_year_end_reminder() else {
        return;
    };

    let usage = prior_year_usage(config, archive_year);
    let body = build_dialog_body(config, &usage);

    let mut state = YearEndReminderState::load().unwrap_or_default();
    if !force_year_end_reminder() {
        state.mark_remind_today(today_local());
        let _ = state.save();
    }

    let dlg = MessageDialog::builder()
        .transient_for(parent)
        .modal(true)
        .message_type(MessageType::Info)
        .text(format!("Archive {} backups?", usage.archive_year))
        .secondary_text(&body)
        .build();

    dlg.add_button("Remind Me Tomorrow", ResponseType::Reject);
    dlg.add_button("Archive Complete", ResponseType::Accept);

    let archive_year = usage.archive_year;
    dlg.connect_response(move |dialog, response| {
        if !force_year_end_reminder() {
            let mut state = YearEndReminderState::load().unwrap_or_default();
            if response == ResponseType::Accept {
                state.mark_dismissed(archive_year);
                let _ = state.save();
            }
        }
        dialog.close();
    });

    dlg.present();
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    #[test]
    fn should_show_only_in_january_before_dismiss() {
        let state = YearEndReminderState::default();
        assert_eq!(
            state.should_show_today(NaiveDate::from_ymd_opt(2026, 1, 2).unwrap()),
            Some(2025)
        );
        assert!(state
            .should_show_today(NaiveDate::from_ymd_opt(2026, 2, 1).unwrap())
            .is_none());
    }

    #[test]
    fn dismiss_stops_prompt_for_that_archive_year() {
        let mut state = YearEndReminderState::default();
        state.mark_dismissed(2025);
        assert!(state
            .should_show_today(NaiveDate::from_ymd_opt(2026, 1, 5).unwrap())
            .is_none());
    }

    #[test]
    fn remind_once_per_day() {
        let mut state = YearEndReminderState::default();
        let day = NaiveDate::from_ymd_opt(2026, 1, 3).unwrap();
        assert_eq!(state.should_show_today(day), Some(2025));
        state.mark_remind_today(day);
        assert!(state.should_show_today(day).is_none());
        assert_eq!(
            state.should_show_today(NaiveDate::from_ymd_opt(2026, 1, 4).unwrap()),
            Some(2025)
        );
    }

    #[test]
    fn snapshot_year_from_name_parses_prefixes() {
        assert_eq!(
            snapshot_year_from_name("full-2025-06-01_020000"),
            Some(2025)
        );
        assert_eq!(snapshot_year_from_name("inc-2024-12-31_235959"), Some(2024));
        assert_eq!(snapshot_year_from_name("latest"), None);
    }
}
