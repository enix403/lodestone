use clap::{Args, Parser, Subcommand};
use lodestone::error::ExitCode;
use lodestone::plan::Direction;
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

/// Flags shared by the three mutating commands.
#[derive(Args)]
struct SyncArgs {
    /// Folder name, or `.` for the folder containing the current directory.
    /// Omit to operate on every configured folder.
    target: Option<String>,

    /// Plan and report, but change nothing
    #[arg(long)]
    dry_run: bool,

    /// Raise the true-delete ceiling for this run only
    #[arg(long, value_name = "N")]
    allow_deletes: Option<usize>,
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

    /// Synchronise in both directions
    Sync(SyncArgs),

    /// Send local changes up; aborts if the remote has changes to bring down
    Push(SyncArgs),

    /// Bring remote changes down; aborts if there are local changes to send up
    Pull(SyncArgs),

    /// Configure a new folder and establish its baseline
    Add {
        /// Short name, used in commands and as the state directory name
        name: String,

        /// Local directory (created if absent). Paths under $HOME are stored as ~/...
        #[arg(long, value_name = "PATH")]
        local: String,

        /// rclone remote and path, e.g. per-gdrive:Silvermine
        #[arg(long, value_name = "REMOTE:PATH")]
        remote: String,

        /// Override the true-delete ceiling for this folder
        #[arg(long, value_name = "N")]
        max_deletes: Option<usize>,

        /// Write the config entry but do not establish the baseline
        #[arg(long)]
        no_init: bool,
    },

    /// Stop managing a folder. Removes config and state; never deletes your files
    Forget {
        name: String,

        /// Leave lodestone's local state (snapshot, bisync workdir) in place
        #[arg(long)]
        keep_state: bool,
    },

    /// Establish the baseline for a folder: resync, then record the snapshot
    Init {
        /// Folder name. Omit to initialise every configured folder.
        target: Option<String>,
    },

    /// Clear a bisync lock left behind by an interrupted run
    Unlock {
        /// Folder name. Omit for every configured folder.
        target: Option<String>,
    },

    /// Inspect and recover files removed from the local side by a sync
    Trash {
        #[command(subcommand)]
        sub: TrashCmd,
    },

    /// Environment and configuration checks
    Doctor {
        #[command(subcommand)]
        sub: Option<DoctorCmd>,
    },
}

#[derive(Subcommand)]
enum TrashCmd {
    /// Show what is recoverable
    List {
        /// Folder name, or `.`. Omit for every configured folder.
        target: Option<String>,
    },
    /// Put a trashed file back into the folder
    Restore {
        folder: String,
        /// Path relative to the folder root, as shown by `lode trash list`
        path: String,
        /// Pick a specific run instead of the most recent copy
        #[arg(long, value_name = "RUN")]
        run: Option<String>,
        /// Replace the file if it already exists
        #[arg(long)]
        overwrite: bool,
    },
    /// Delete old trash runs
    Prune {
        /// Folder name, or `.`. Omit for every configured folder.
        target: Option<String>,
        /// Age threshold in days (default 30)
        #[arg(long, value_name = "DAYS")]
        older_than: Option<u64>,
        /// Delete every run regardless of age
        #[arg(long)]
        all: bool,
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
        Cmd::Sync(a) => mutate(cli, a, Direction::Both),
        Cmd::Push(a) => mutate(cli, a, Direction::Push),
        Cmd::Pull(a) => mutate(cli, a, Direction::Pull),
        Cmd::Add {
            name,
            local,
            remote,
            max_deletes,
            no_init,
        } => cmd::add::run(
            cli.config.as_deref(),
            &cmd::add::Options {
                name,
                local,
                remote,
                max_deletes: *max_deletes,
                no_init: *no_init,
            },
        ),
        Cmd::Forget { name, keep_state } => {
            cmd::forget::run(cli.config.as_deref(), name, *keep_state)
        }
        Cmd::Init { target } => {
            let cfg = Config::load(cli.config.as_deref())?;
            cmd::init::run(&cfg, target.as_deref())
        }
        Cmd::Unlock { target } => {
            let cfg = Config::load(cli.config.as_deref())?;
            cmd::unlock::run(&cfg, target.as_deref())
        }
        Cmd::Trash { sub } => {
            let cfg = Config::load(cli.config.as_deref())?;
            match sub {
                TrashCmd::List { target } => cmd::trash::list(&cfg, target.as_deref(), cli.json),
                TrashCmd::Restore {
                    folder,
                    path,
                    run,
                    overwrite,
                } => cmd::trash::restore(&cfg, folder, path, run.as_deref(), *overwrite),
                TrashCmd::Prune {
                    target,
                    older_than,
                    all,
                } => cmd::trash::prune(&cfg, target.as_deref(), *older_than, *all),
            }
        }
        Cmd::Doctor { sub } => match sub {
            None => cmd::doctor::run(cli.config.as_deref()),
            Some(DoctorCmd::RenameTest) => cmd::rename_test::run(),
        },
    }
}

fn mutate(cli: &Cli, a: &SyncArgs, direction: Direction) -> Result<ExitCode> {
    let cfg = Config::load(cli.config.as_deref())?;
    cmd::apply::run(
        &cfg,
        a.target.as_deref(),
        &cmd::apply::Options {
            direction,
            json: cli.json,
            dry_run: a.dry_run,
            allow_deletes: a.allow_deletes,
        },
    )
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
