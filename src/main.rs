#![allow(deprecated)] // GTK4 4.10 deprecations: ComboBoxText, FileChooserDialog, MessageDialog
mod backup;
mod config;
mod drives;
mod systemd;
mod ui;

use anyhow::{Context, Result};
use backup::BackupKind;
use config::Config;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();

    // ── Headless backup mode ───────────────────────────────────────────────
    // Called by the systemd service: `home-backup --backup auto`
    if let Some(pos) = args.iter().position(|a| a == "--backup") {
        let kind_str = args.get(pos + 1).map(|s| s.as_str()).unwrap_or("auto");
        let kind: BackupKind = kind_str
            .parse()
            .with_context(|| format!("invalid backup kind '{kind_str}'"))?;

        let cfg = Config::load().context("loading config")?.with_context(|| {
            format!(
                "No config found at {}.  Run the GUI first to set up backups.",
                Config::config_path().display()
            )
        })?;

        let summary = backup::run(&cfg, kind).context("running backup")?;

        println!("{summary}");
        return Ok(());
    }

    // ── GUI mode ───────────────────────────────────────────────────────────
    let config = Config::load().unwrap_or(None);
    ui::run_app(config);

    Ok(())
}
