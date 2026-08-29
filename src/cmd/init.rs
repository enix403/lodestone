//! `lode init` — establish the baseline for a folder.
//!
//! `--resync` is bisync's most dangerous primitive: it declares a new baseline, and used
//! at the wrong moment it is how people lose data. lodestone confines it to this command
//! and to an explicit `lode resync --i-understand`; it is never reached automatically,
//! and `--resilient` (which lets bisync self-heal in ways you cannot audit) is never
//! passed.

use lodestone::config::{Config, Folder};
use lodestone::error::ExitCode;
use lodestone::snapshot::Snapshot;
use lodestone::{machine, paths, rclone::Rclone, Error, Result};

pub fn run(cfg: &Config, target: Option<&str>) -> Result<ExitCode> {
    let folders = crate::resolve_targets(cfg, target)?;
    let rclone = Rclone::discover()?;
    rclone.require_min_version()?;
    let machine_id = machine::machine_id()?;
    let remotes = rclone.list_remotes()?;

    for f in folders {
        if Snapshot::exists(&f.name) {
            println!(
                "{}: already initialised (use `lode resync` to rebaseline)",
                f.name
            );
            continue;
        }
        init_one(&rclone, &remotes, &machine_id, f)?;
    }
    Ok(ExitCode::Ok)
}

fn init_one(rclone: &Rclone, remotes: &[String], machine_id: &str, f: &Folder) -> Result<()> {
    // Validate the remote before doing anything destructive. rclone owns rclone.conf;
    // lodestone only checks that the name it was given actually resolves.
    if let Some(name) = f.remote_name() {
        if !remotes.iter().any(|r| r == name) {
            return Err(Error::UnknownRemote(name.to_string()));
        }
    }

    std::fs::create_dir_all(&f.local).map_err(|e| Error::io(f.local.display(), e))?;
    let workdir = paths::bisync_workdir(&f.name);
    std::fs::create_dir_all(&workdir).map_err(|e| Error::io(workdir.display(), e))?;

    let local_path = f.local.display().to_string();
    println!(
        "{}: establishing baseline ({} <-> {})",
        f.name, local_path, f.remote
    );

    rclone.bisync(
        &local_path,
        &f.remote,
        &workdir.display().to_string(),
        &["--resync"],
    )?;

    // After a successful resync both sides agree, so either listing is a valid merge
    // base. Read the local one: it is cheap and cannot be affected by remote latency.
    let entries = rclone.lsjson(&local_path)?;
    let count = entries.len();
    Snapshot::new(&f.name, machine_id, entries).save()?;

    println!("{}: baseline recorded, {count} file(s)", f.name);
    Ok(())
}
