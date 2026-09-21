#![allow(deprecated)] // GTK4 4.10 deprecations: ComboBoxText, FileChooserDialog, MessageDialog
mod backup;
mod config;
mod drives;
mod pending_full;
mod run_lock;
mod systemd;
mod ui;
mod year_end;

use anyhow::{Context, Result};
use backup::BackupKind;
use clap::{Parser, Subcommand};
use config::Config;

/// Backup-Tool — GTK4 rsync backup manager using rsync and systemd.
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
    /// KIND controls what kind of backup is created:
    ///
    ///   auto         Choose automatically: full on the configured day-of-week
    ///                or when no full backup exists; incremental otherwise.
    ///                This is what the systemd timer uses.
    ///
    ///   full         Always create a full backup (every file is copied).
    ///
    ///   incremental  Always create an incremental backup (only changed
    ///                files are copied; unchanged files are hardlinked from
    ///                the previous backup).
    Backup {
        /// Kind of backup to run: auto, full, or incremental (default: auto).
        #[arg(default_value = "auto")]
        kind: String,
    },
    /// Permanently reclaim space (never trash — direct delete).
    Prune {
        #[command(subcommand)]
        target: PruneTarget,
    },
}

#[derive(Debug, Subcommand)]
enum PruneTarget {
    /// Permanently delete all inc-* backups on the backup drive.
    Incs,
    /// Permanently empty Trash on the backup volume (.Trash-* / Nautilus deletes).
    Trash,
    /// Empty backup-volume trash, then vaporize all inc-* backups.
    All,
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

        Some(Commands::Prune { target }) => {
            let cfg = Config::load().context("loading config")?.with_context(|| {
                format!(
                    "No config found at {}. Run the GUI first to set up backups.",
                    Config::config_path().display()
                )
            })?;

            let msg = match target {
                PruneTarget::Incs => backup::vaporize_incrementals(&cfg),
                PruneTarget::Trash => backup::vaporize_volume_trash(&cfg),
                PruneTarget::All => {
                    let trash = backup::vaporize_volume_trash(&cfg)?;
                    let incs = backup::vaporize_incrementals(&cfg)?;
                    Ok(format!("{trash}\n{incs}"))
                }
            }?;
            println!("{msg}");
        }

        // ── GUI mode ───────────────────────────────────────────────────────
        None => {
            let config = Config::load().unwrap_or(None);
            ui::run_app(config);
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::error::ErrorKind;

    // ── CLI argument parsing ───────────────────────────────────────────────

    #[test]
    fn cli_no_args_opens_gui() {
        let cli = Cli::try_parse_from(["backup-tool"]).unwrap();
        assert!(cli.command.is_none());
    }

    #[test]
    fn cli_backup_subcommand_defaults_to_auto() {
        let cli = Cli::try_parse_from(["backup-tool", "backup"]).unwrap();
        match cli.command {
            Some(Commands::Backup { kind }) => assert_eq!(kind, "auto"),
            _ => panic!("expected Backup command"),
        }
    }

    #[test]
    fn cli_backup_full() {
        let cli = Cli::try_parse_from(["backup-tool", "backup", "full"]).unwrap();
        match cli.command {
            Some(Commands::Backup { kind }) => assert_eq!(kind, "full"),
            _ => panic!("expected Backup command"),
        }
    }

    #[test]
    fn cli_backup_incremental() {
        let cli = Cli::try_parse_from(["backup-tool", "backup", "incremental"]).unwrap();
        match cli.command {
            Some(Commands::Backup { kind }) => assert_eq!(kind, "incremental"),
            _ => panic!("expected Backup command"),
        }
    }

    #[test]
    fn cli_backup_inc_alias() {
        // "inc" is accepted by BackupKind::from_str; clap passes it through as-is.
        let cli = Cli::try_parse_from(["backup-tool", "backup", "inc"]).unwrap();
        match cli.command {
            Some(Commands::Backup { kind }) => assert_eq!(kind, "inc"),
            _ => panic!("expected Backup command"),
        }
    }

    #[test]
    fn cli_help_flag_returns_display_help_error() {
        let err = Cli::try_parse_from(["backup-tool", "--help"]).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::DisplayHelp);
    }

    #[test]
    fn cli_version_flag_returns_display_version_error() {
        let err = Cli::try_parse_from(["backup-tool", "--version"]).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::DisplayVersion);
    }

    #[test]
    fn cli_unknown_subcommand_is_an_error() {
        assert!(Cli::try_parse_from(["backup-tool", "unknown"]).is_err());
    }

    #[test]
    fn cli_extra_positional_arg_is_an_error() {
        assert!(Cli::try_parse_from(["backup-tool", "stray-arg"]).is_err());
    }

    #[test]
    fn cli_prune_incs() {
        let cli = Cli::try_parse_from(["backup-tool", "prune", "incs"]).unwrap();
        match cli.command {
            Some(Commands::Prune {
                target: PruneTarget::Incs,
            }) => {}
            _ => panic!("expected prune incs"),
        }
    }
}
