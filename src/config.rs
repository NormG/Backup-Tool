use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Application configuration stored under `$XDG_CONFIG_HOME/backup-tool/config.toml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Absolute path of the directory to back up.
    pub source_dir: String,
    /// Absolute path on the backup drive where snapshots are written.
    pub dest_dir: String,
    /// Filesystem UUID of the backup partition (used for auto-mount).
    pub drive_uuid: Option<String>,
    /// Human-readable label of the backup partition (informational only).
    pub drive_label: Option<String>,
    /// Day of week that triggers a full backup (e.g. "Monday").
    pub full_backup_day: String,
    /// Local time for the daily backup in `HH:MM` format.
    pub backup_time: String,
    /// rsync exclude patterns, one per entry.
    pub excludes: Vec<String>,
    /// Safety net: delete orphaned incrementals older than this many days when
    /// a scheduled full backup has not run yet.  Incrementals from the current
    /// cycle are removed automatically when the next full succeeds.
    pub retention_days: u32,
    /// How many days must pass between incremental backups (1 = every day).
    /// Old config files that omit this field default to 1.
    #[serde(default = "default_one")]
    pub incremental_every_n_days: u32,
    /// How many full snapshots to keep on the backup drive (0 = unlimited).
    /// When a new full succeeds, older full-* directories beyond this count
    /// are deleted automatically.  Omitted in legacy configs defaults to 0
    /// (unlimited); new installs via the wizard use 12.
    #[serde(default = "default_zero")]
    pub keep_full_snapshots: u32,
    /// Set when a scheduled full was deferred for lack of space; auto mode
    /// retries a full on subsequent runs until one succeeds.
    #[serde(default)]
    pub pending_full_backup: bool,
    /// True after a successful first install so the wizard is not shown again.
    pub installed: bool,
}

impl Default for Config {
    fn default() -> Self {
        let home = dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("/home/user"))
            .to_string_lossy()
            .into_owned();
        Self {
            source_dir: home,
            dest_dir: String::new(),
            drive_uuid: None,
            drive_label: None,
            full_backup_day: "Monday".to_string(),
            backup_time: "02:00".to_string(),
            excludes: vec![
                ".cache/".to_string(),
                ".thumbnails/".to_string(),
                ".var/app/*/cache/".to_string(),
                ".mozilla/firefox/*/cache2/".to_string(),
                ".config/google-chrome/*/Cache/".to_string(),
                ".config/chromium/*/Cache/".to_string(),
                ".local/share/Trash/".to_string(),
                ".Trash-*/".to_string(),
                "*.iso".to_string(),
                ".extras/".to_string(),
                "lost+found/".to_string(),
                ".gvfs/".to_string(),
                ".cargo/".to_string(),
                "*~".to_string(),
            ],
            retention_days: 30,
            incremental_every_n_days: 1,
            keep_full_snapshots: 12,
            pending_full_backup: false,
            installed: false,
        }
    }
}

impl Config {
    /// Returns the path to the config file.
    pub fn config_path() -> PathBuf {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from(".config"))
            .join("backup-tool")
            .join("config.toml")
    }

    /// Returns the path to the data directory (logs, exclude lists, etc.).
    pub fn data_dir() -> PathBuf {
        dirs::data_local_dir()
            .unwrap_or_else(|| PathBuf::from(".local/share"))
            .join("backup-tool")
    }

    pub fn log_path() -> PathBuf {
        Self::data_dir().join("backup.log")
    }

    /// Legacy config path from before the home-backup → backup-tool rename.
    fn legacy_config_path() -> PathBuf {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from(".config"))
            .join("home-backup")
            .join("config.toml")
    }

    /// Load config from disk, returning `None` if the file does not yet exist.
    pub fn load() -> Result<Option<Self>> {
        let path = Self::config_path();
        let legacy = Self::legacy_config_path();
        let read_path = if path.exists() {
            path
        } else if legacy.exists() {
            legacy
        } else {
            return Ok(None);
        };
        let raw = std::fs::read_to_string(&read_path)
            .with_context(|| format!("reading config {}", read_path.display()))?;
        let cfg: Self = toml::from_str(&raw).with_context(|| "parsing config.toml")?;
        Ok(Some(cfg))
    }

    /// Persist config to disk, creating parent directories as needed.
    pub fn save(&self) -> Result<()> {
        let mut to_save = self.clone();
        if let Ok(Some(on_disk)) = Self::load() {
            to_save.pending_full_backup =
                preserve_pending_full_backup(self.pending_full_backup, on_disk.pending_full_backup);
        }

        let path = Self::config_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating config dir {}", parent.display()))?;
        }
        let raw = toml::to_string_pretty(&to_save).context("serialising config to TOML")?;
        std::fs::write(&path, raw).with_context(|| format!("writing config {}", path.display()))?;
        Ok(())
    }

    /// Parse backup hour and minute from `backup_time` ("HH:MM").
    /// Returns `(hour, minute)`.  Falls back to `(2, 0)` on parse failure.
    pub fn backup_hm(&self) -> (u8, u8) {
        let parts: Vec<&str> = self.backup_time.splitn(2, ':').collect();
        let h = parts.first().and_then(|s| s.parse().ok()).unwrap_or(2u8);
        let m = parts.get(1).and_then(|s| s.parse().ok()).unwrap_or(0u8);
        (h, m)
    }

    /// Human-readable label for `keep_full_snapshots` (0 → "unlimited").
    pub fn keep_full_snapshots_label(&self) -> String {
        if self.keep_full_snapshots == 0 {
            "unlimited".to_string()
        } else {
            self.keep_full_snapshots.to_string()
        }
    }

    /// Persist `pending_full_backup` so deferred fulls are retried on later runs.
    pub fn set_pending_full_backup(pending: bool) -> Result<()> {
        let mut cfg = Self::load()?.with_context(|| {
            format!(
                "loading config to update pending_full_backup ({})",
                Self::config_path().display()
            )
        })?;
        if cfg.pending_full_backup == pending {
            return Ok(());
        }
        cfg.pending_full_backup = pending;
        cfg.save()
    }
}

// Used by #[serde(default = "default_one")] on Config::incremental_every_n_days.
pub fn default_one() -> u32 {
    1
}

// Used by #[serde(default = "default_zero")] on Config::keep_full_snapshots.
pub fn default_zero() -> u32 {
    0
}

fn preserve_pending_full_backup(in_memory: bool, on_disk: bool) -> bool {
    in_memory || on_disk
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── backup_hm ────────────────────────────────────────────────────────────

    #[test]
    fn backup_hm_parses_hh_mm() {
        let cfg = Config {
            backup_time: "02:30".to_string(),
            ..Config::default()
        };
        assert_eq!(cfg.backup_hm(), (2, 30));
    }

    #[test]
    fn backup_hm_parses_midnight() {
        let cfg = Config {
            backup_time: "00:00".to_string(),
            ..Config::default()
        };
        assert_eq!(cfg.backup_hm(), (0, 0));
    }

    #[test]
    fn backup_hm_parses_end_of_day() {
        let cfg = Config {
            backup_time: "23:59".to_string(),
            ..Config::default()
        };
        assert_eq!(cfg.backup_hm(), (23, 59));
    }

    #[test]
    fn backup_hm_fallback_on_invalid_string() {
        let cfg = Config {
            backup_time: "not-a-time".to_string(),
            ..Config::default()
        };
        assert_eq!(cfg.backup_hm(), (2, 0));
    }

    #[test]
    fn backup_hm_fallback_on_empty_string() {
        let cfg = Config {
            backup_time: String::new(),
            ..Config::default()
        };
        assert_eq!(cfg.backup_hm(), (2, 0));
    }

    // ── TOML round-trip ───────────────────────────────────────────────────────

    #[test]
    fn config_toml_round_trip_preserves_all_fields() {
        let original = Config {
            source_dir: "/home/test".to_string(),
            dest_dir: "/mnt/backup".to_string(),
            drive_uuid: Some("abc-1234".to_string()),
            drive_label: Some("My Drive".to_string()),
            full_backup_day: "Friday".to_string(),
            backup_time: "03:15".to_string(),
            excludes: vec![".cache/".to_string(), "*.iso".to_string()],
            retention_days: 14,
            incremental_every_n_days: 2,
            keep_full_snapshots: 6,
            pending_full_backup: true,
            installed: true,
        };
        let toml_str = toml::to_string_pretty(&original).unwrap();
        let restored: Config = toml::from_str(&toml_str).unwrap();

        assert_eq!(restored.source_dir, original.source_dir);
        assert_eq!(restored.dest_dir, original.dest_dir);
        assert_eq!(restored.drive_uuid, original.drive_uuid);
        assert_eq!(restored.drive_label, original.drive_label);
        assert_eq!(restored.full_backup_day, original.full_backup_day);
        assert_eq!(restored.backup_time, original.backup_time);
        assert_eq!(restored.excludes, original.excludes);
        assert_eq!(restored.retention_days, original.retention_days);
        assert_eq!(
            restored.incremental_every_n_days,
            original.incremental_every_n_days
        );
        assert_eq!(restored.keep_full_snapshots, original.keep_full_snapshots);
        assert_eq!(restored.pending_full_backup, original.pending_full_backup);
        assert_eq!(restored.installed, original.installed);
    }

    #[test]
    fn config_optional_drive_fields_can_be_absent() {
        let cfg = Config {
            drive_uuid: None,
            drive_label: None,
            ..Config::default()
        };
        let toml_str = toml::to_string_pretty(&cfg).unwrap();
        let restored: Config = toml::from_str(&toml_str).unwrap();
        assert!(restored.drive_uuid.is_none());
        assert!(restored.drive_label.is_none());
    }

    /// Old config files that pre-date `incremental_every_n_days` should
    /// deserialise with a default of 1.
    #[test]
    fn config_missing_incremental_field_defaults_to_one() {
        let toml_str = r#"
source_dir = "/home/test"
dest_dir = "/mnt/backup"
full_backup_day = "Monday"
backup_time = "02:00"
excludes = []
retention_days = 30
installed = false
"#;
        let cfg: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(cfg.incremental_every_n_days, 1);
        assert_eq!(cfg.keep_full_snapshots, 0);
        assert!(!cfg.pending_full_backup);
    }

    // ── default_one / default_zero ────────────────────────────────────────────

    #[test]
    fn default_one_returns_one() {
        assert_eq!(default_one(), 1);
    }

    #[test]
    fn default_zero_returns_zero() {
        assert_eq!(default_zero(), 0);
    }

    #[test]
    fn preserve_pending_full_backup_keeps_disk_retry() {
        assert!(preserve_pending_full_backup(false, true));
        assert!(!preserve_pending_full_backup(false, false));
        assert!(preserve_pending_full_backup(true, false));
    }
}
