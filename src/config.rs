use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Application configuration stored under `$XDG_CONFIG_HOME/home-backup/config.toml`.
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
    /// Number of days incremental backups are kept before deletion.
    pub retention_days: u32,
    /// How many days must pass between incremental backups (1 = every day).
    /// Old config files that omit this field default to 1.
    #[serde(default = "default_one")]
    pub incremental_every_n_days: u32,
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
            installed: false,
        }
    }
}

impl Config {
    /// Returns the path to the config file.
    pub fn config_path() -> PathBuf {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from(".config"))
            .join("home-backup")
            .join("config.toml")
    }

    /// Returns the path to the data directory (logs, exclude lists, etc.).
    pub fn data_dir() -> PathBuf {
        dirs::data_local_dir()
            .unwrap_or_else(|| PathBuf::from(".local/share"))
            .join("home-backup")
    }

    pub fn log_path() -> PathBuf {
        Self::data_dir().join("backup.log")
    }

    /// Load config from disk, returning `None` if the file does not yet exist.
    pub fn load() -> Result<Option<Self>> {
        let path = Self::config_path();
        if !path.exists() {
            return Ok(None);
        }
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("reading config {}", path.display()))?;
        let cfg: Self = toml::from_str(&raw).with_context(|| "parsing config.toml")?;
        Ok(Some(cfg))
    }

    /// Persist config to disk, creating parent directories as needed.
    pub fn save(&self) -> Result<()> {
        let path = Self::config_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating config dir {}", parent.display()))?;
        }
        let raw = toml::to_string_pretty(self).context("serialising config to TOML")?;
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
}

// Used by #[serde(default = "default_one")] on Config::incremental_every_n_days.
pub fn default_one() -> u32 {
    1
}
