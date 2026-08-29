//! `lode add` — configure a folder and baseline it in one command.
//!
//! This is the onboarding path: on a new machine, `rclone config` the remote once, then
//! one `lode add` per folder. The remote is validated *before* the config is written, so a
//! typo leaves nothing behind to clean up.

use lodestone::config::Config;
use lodestone::configfile::{self, NewFolder};
use lodestone::error::ExitCode;
use lodestone::session::Session;
use lodestone::{Error, Result};
use std::path::Path;

pub struct Options<'a> {
    pub name: &'a str,
    pub local: &'a str,
    pub remote: &'a str,
    pub max_deletes: Option<usize>,
    pub no_init: bool,
}

pub fn run(config: Option<&Path>, opts: &Options<'_>) -> Result<ExitCode> {
    let target = configfile::target_path(config);

    // Fail before touching the config file, not after.
    if let Ok(existing) = Config::load(config) {
        if existing.get(opts.name).is_ok() {
            return Err(Error::Config(format!(
                "folder {:?} is already configured",
                opts.name
            )));
        }
    }

    let session = Session::new()?;
    if let Some((remote_name, _)) = opts.remote.split_once(':') {
        if !remote_name.is_empty() {
            let remotes = session.rclone.list_remotes()?;
            if !remotes.iter().any(|r| r == remote_name) {
                return Err(Error::UnknownRemote(remote_name.to_string()));
            }
        }
    }

    let local = configfile::normalise_input(opts.local)?;
    configfile::add_folder(
        &target,
        &NewFolder {
            name: opts.name,
            local: &local,
            remote: opts.remote,
            max_deletes: opts.max_deletes,
        },
    )?;
    println!(
        "{}: added to {} ({local} <-> {})",
        opts.name,
        target.display(),
        opts.remote
    );

    if opts.no_init {
        println!(
            "{}: not initialised — run `lode init {}`",
            opts.name, opts.name
        );
        return Ok(ExitCode::Ok);
    }

    // Re-load so the new stanza goes through the same validation as any other folder.
    let cfg = Config::load(config)?;
    crate::cmd::init::run(&cfg, Some(opts.name))
}
