mod backup;
mod config;
mod doctor;
mod install;
mod state;
mod status;
mod util;

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};

use crate::config::{Config, INSTALLED_CONFIG_PATH};

#[derive(Debug, Parser)]
#[command(
    name = "silo-keeper",
    version,
    about = "A production backup reserve for PostgreSQL and S3"
)]
struct Cli {
    #[arg(long, global = true, default_value = INSTALLED_CONFIG_PATH)]
    config: PathBuf,
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Validate the TOML configuration and its filesystem permissions.
    Check,
    /// Check runtime commands and connectivity, optionally exercising real backup permissions.
    Doctor {
        /// Run a full pg_dump to /dev/null and upload a retained tiny S3 canary object.
        #[arg(long)]
        canary: bool,
    },
    /// Show backup and systemd timer status.
    Status {
        target: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Run one backup target immediately.
    Run { target: String },
    #[command(hide = true, name = "run-scheduled")]
    RunScheduled { target: String },
    /// Show local execution history for one target.
    History {
        target: String,
        #[arg(long, default_value_t = 20)]
        limit: usize,
        #[arg(long)]
        json: bool,
    },
    /// Install the binary, root-only config, and systemd timers.
    Install {
        #[arg(long)]
        install_dependencies: bool,
    },
    /// Stop and remove systemd scheduling. Remote backups are never deleted.
    Uninstall {
        #[arg(long)]
        purge_local: bool,
    },
}

fn main() {
    if let Err(error) = run() {
        eprintln!("error: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Install {
            install_dependencies,
        } => {
            let config = Config::load_install_source(&cli.config)?;
            install::install(&config, &cli.config, install_dependencies)
        }
        Commands::Uninstall { purge_local } => install::uninstall(purge_local),
        command => {
            let config = Config::load(&cli.config)?;
            match command {
                Commands::Check => {
                    println!(
                        "configuration {} is valid for {} target(s)",
                        cli.config.display(),
                        config.targets.len()
                    );
                    Ok(())
                }
                Commands::Doctor { canary } => doctor::run(&config, canary),
                Commands::Status { target, json } => status::show(&config, target.as_deref(), json),
                Commands::Run { target } => {
                    let target = config.target(&target)?;
                    backup::run(&config, target)
                }
                Commands::RunScheduled { target } => {
                    let target = config.target(&target)?;
                    backup::run_scheduled(&config, target)
                }
                Commands::History {
                    target,
                    limit,
                    json,
                } => status::history(&config, &target, limit, json),
                Commands::Install { .. } | Commands::Uninstall { .. } => unreachable!(),
            }
        }
    }
}
