#![allow(deprecated)] // GTK4 4.10 deprecations: ComboBoxText, FileChooserDialog, MessageDialog
mod backup;
mod config;
mod drives;
mod systemd;
mod ui;

use anyhow::{Context, Result};
use backup::BackupKind;
use clap::{Parser, Subcommand};
use config::Config;

/// GTK4 home-directory backup manager using rsync and systemd.
///
/// Run without arguments to open the graphical interface.
/// Use the `backup` subcommand to run a headless backup from the
/// terminal or systemd.
#[derive(Debug, Parser)]
#[command(
    name = "backup-tool",
    version,
    about,
    long_about = None,
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Run a backup without opening the GUI.
    ///
    /// KIND controls what kind of snapshot is created:
    ///
    ///   auto         Choose automatically: full on the configured day-of-week
    ///                or when no full snapshot exists; incremental otherwise.
    ///                This is what the systemd timer uses.
    ///
    ///   full         Always create a full snapshot (every file is copied).
    ///
    ///   incremental  Always create an incremental snapshot (only changed
    ///                files are copied; unchanged files are hardlinked from
    ///                the previous snapshot).
    Backup {
        /// Kind of backup to run: auto, full, or incremental (default: auto).
        #[arg(default_value = "auto")]
        kind: String,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        // ── Headless backup mode ───────────────────────────────────────────
        Some(Commands::Backup { kind: kind_str }) => {
            let kind: BackupKind = kind_str
                .parse()
                .with_context(|| format!("invalid backup kind '{kind_str}'"))?;

            let cfg = Config::load().context("loading config")?.with_context(|| {
                format!(
                    "No config found at {}. Run the GUI first to set up backups.",
                    Config::config_path().display()
                )
            })?;

            let summary = backup::run(&cfg, kind).context("running backup")?;
            println!("{summary}");
        }

        // ── GUI mode ───────────────────────────────────────────────────────
        None => {
            let config = Config::load().unwrap_or(None);
            ui::run_app(config);
        }
    }

    Ok(())
}
