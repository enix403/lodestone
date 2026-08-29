//! `lode forget` — stop managing a folder.
//!
//! Named for its semantics. It removes the configuration and lodestone's local state, and
//! it **never touches your files** on either side. Typed on the wrong folder half-asleep,
//! failing safe is the difference between an annoyance and a bad evening — so the output
//! says plainly what was and was not removed.

use lodestone::config::Config;
use lodestone::configfile;
use lodestone::error::ExitCode;
use lodestone::{paths, Error, Result};
use std::path::Path;

pub fn run(config: Option<&Path>, name: &str, keep_state: bool) -> Result<ExitCode> {
    let cfg = Config::load(config)?;
    let folder = cfg.get(name)?;
    let local = folder.local.clone();
    let remote = folder.remote.clone();

    let target = configfile::target_path(config);
    if !configfile::remove_folder(&target, name)? {
        return Err(Error::Config(format!(
            "folder {name:?} is not defined in {} (it may come from config.local.toml)",
            target.display()
        )));
    }
    println!("{name}: removed from {}", target.display());

    if keep_state {
        println!("{name}: local state kept (--keep-state)");
    } else {
        for dir in [paths::folder_state_dir(name), paths::bisync_workdir(name)] {
            if dir.exists() {
                std::fs::remove_dir_all(&dir).map_err(|e| Error::io(dir.display(), e))?;
                println!("{name}: removed state {}", dir.display());
            }
        }
    }

    println!(
        "{name}: no files were deleted — {} and {remote} are untouched",
        local.display()
    );
    Ok(ExitCode::Ok)
}
