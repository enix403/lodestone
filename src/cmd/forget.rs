//! `lode forget` — stop managing a folder.
//!
//! Named for its semantics. It removes the configuration and lodestone's local state, and
//! it **never touches your files** on either side. Typed on the wrong folder half-asleep,
//! failing safe is the difference between an annoyance and a bad evening — so the output
//! says plainly what was and was not removed.

use lodestone::config::Config;
use lodestone::configfile;
use lodestone::error::ExitCode;
use lodestone::{paths, trash, Error, Result};
use std::path::Path;

pub fn run(
    config: Option<&Path>,
    name: &str,
    keep_state: bool,
    purge_trash: bool,
) -> Result<ExitCode> {
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

    // The trash is handled separately from the rest of the state, because it is the only
    // part that can hold data with no other copy: a file deleted on another machine and
    // caught here may exist nowhere else. Removing it silently would be a data loss bug.
    let remaining = trash::list(name);
    let trash_root = trash::root(name);
    if purge_trash {
        if trash_root.exists() {
            std::fs::remove_dir_all(&trash_root).map_err(|e| Error::io(trash_root.display(), e))?;
            println!(
                "{name}: purged trash ({} file(s)) from {}",
                remaining.len(),
                trash_root.display()
            );
        }
    } else if !remaining.is_empty() {
        println!(
            "{name}: KEPT {} trashed file(s) in {}",
            remaining.len(),
            trash_root.display()
        );
        println!("  these may be the only copy of files deleted elsewhere.");
        println!("  remove them with `lode forget {name} --purge-trash` once you are sure.");
    } else if trash_root.exists() {
        // Empty: nothing to lose, so do not leave a stray directory behind.
        std::fs::remove_dir_all(&trash_root).map_err(|e| Error::io(trash_root.display(), e))?;
    }

    println!(
        "{name}: no files were deleted — {} and {remote} are untouched",
        local.display()
    );
    Ok(ExitCode::Ok)
}
