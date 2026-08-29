use clap::{Parser, Subcommand};
use lodestone::error::ExitCode;
use lodestone::{config::Config, Error, Result};
use std::path::PathBuf;

mod cmd;

#[derive(Parser)]
#[command(
    name = "lode",
    version,
    about = "Manual, git-like bidirectional folder sync on top of rclone bisync",
    long_about = "lodestone keeps folders in sync with a cloud remote via `rclone bisync`.\n\
                  There is no daemon: you run a command when you have made changes, or when\n\
                  you want to collect changes made elsewhere.\n\n\
                  Every mutating command plans first and aborts on anything surprising."
)]
struct Cli {
    /// Use an explicit config file instead of ~/.config/lode/config.toml
    #[arg(long, global = true, value_name = "PATH")]
    config: Option<PathBuf>,

    /// Emit machine-readable JSON
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// List configured folders
    Folders,

    /// Show pending changes without touching anything (the plan phase, run alone)
    Status {
        /// Folder name, or `.` for the folder containing the current directory.
        /// Omit to check every configured folder.
        target: Option<String>,
    },

    /// Establish the baseline for a folder: resync, then record the snapshot
    Init {
        /// Folder name. Omit to initialise every configured folder.
        target: Option<String>,
    },

    /// Environment and configuration checks
    Doctor {
        #[command(subcommand)]
        sub: Option<DoctorCmd>,
    },
}

#[derive(Subcommand)]
enum DoctorCmd {
    /// Verify empirically that this rclone collapses moves into server-side renames
    RenameTest,
}

fn main() {
    let cli = Cli::parse();
    let code = match run(&cli) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e}");
            e.exit_code()
        }
    };
    std::process::exit(code.as_i32());
}

fn run(cli: &Cli) -> Result<ExitCode> {
    match &cli.command {
        Cmd::Folders => {
            let cfg = Config::load(cli.config.as_deref())?;
            cmd::folders::run(&cfg, cli.json)
        }
        Cmd::Status { target } => {
            let cfg = Config::load(cli.config.as_deref())?;
            cmd::status::run(&cfg, target.as_deref(), cli.json)
        }
        Cmd::Init { target } => {
            let cfg = Config::load(cli.config.as_deref())?;
            cmd::init::run(&cfg, target.as_deref())
        }
        Cmd::Doctor { sub } => match sub {
            None => cmd::doctor::run(cli.config.as_deref()),
            Some(DoctorCmd::RenameTest) => cmd::rename_test::run(),
        },
    }
}

/// Resolve a CLI target into the folders to operate on.
///
/// `None` fans out over every configured folder — the common case, since the whole point
/// of `lode pull` with no argument is not visiting each folder by hand.
pub(crate) fn resolve_targets<'a>(
    cfg: &'a Config,
    target: Option<&str>,
) -> Result<Vec<&'a lodestone::config::Folder>> {
    match target {
        None => {
            if cfg.folders.is_empty() {
                return Err(Error::Config(
                    "no folders configured. Add one to ~/.config/lode/config.toml".into(),
                ));
            }
            Ok(cfg.folders.iter().collect())
        }
        Some(".") => {
            let cwd = std::env::current_dir().map_err(|e| Error::io("cwd", e))?;
            Ok(vec![cfg.containing(&cwd)?])
        }
        Some(name) => Ok(vec![cfg.get(name)?]),
    }
}
