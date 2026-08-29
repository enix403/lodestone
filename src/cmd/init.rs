//! `lode init` — establish the baseline for a folder.
//!
//! `--resync` is bisync's most dangerous primitive: it declares a new baseline, and used
//! at the wrong moment it is how people lose data. lodestone confines it to this command
//! and to an explicit `lode resync --i-understand`; it is never reached automatically,
//! and `--resilient` (which lets bisync self-heal in ways you cannot audit) is never
//! passed.
//!
//! This goes through [`Session`] rather than talking to rclone directly, so the baseline
//! is established under exactly the same filter set that every later run uses. Baselining
//! unfiltered and then syncing filtered would make bisync demand a resync on the very
//! next command.

use lodestone::config::{Config, Folder};
use lodestone::error::ExitCode;
use lodestone::session::Session;
use lodestone::snapshot::Snapshot;
use lodestone::{paths, Error, Result};

pub fn run(cfg: &Config, target: Option<&str>) -> Result<ExitCode> {
    let folders = crate::resolve_targets(cfg, target)?;
    let session = Session::new()?;
    let remotes = session.rclone.list_remotes()?;

    for f in folders {
        if Snapshot::exists(&f.name) {
            println!(
                "{}: already initialised (use `lode resync` to rebaseline)",
                f.name
            );
            continue;
        }
        // A folder with bisync listings but no snapshot was initialised here before and
        // then lost its merge base. Resyncing it unions both sides, which resurrects
        // anything deleted or moved locally but never synced — so that case has to be a
        // deliberate `resync`, not an incidental `init`.
        if has_bisync_listings(&f.name) {
            return Err(Error::PreviouslyInitialised(f.name.clone()));
        }
        establish_baseline(&session, &remotes, f)?;
    }
    Ok(ExitCode::Ok)
}

/// Whether bisync has prior listings for this folder, i.e. it ran here before.
fn has_bisync_listings(folder: &str) -> bool {
    std::fs::read_dir(paths::bisync_workdir(folder))
        .map(|rd| {
            rd.flatten()
                .any(|e| e.path().extension().is_some_and(|x| x == "lst"))
        })
        .unwrap_or(false)
}

pub fn establish_baseline(session: &Session, remotes: &[String], f: &Folder) -> Result<()> {
    // Validate the remote before doing anything destructive. rclone owns rclone.conf;
    // lodestone only checks that the name it was given actually resolves.
    if let Some(name) = f.remote_name() {
        if !remotes.iter().any(|r| r == name) {
            return Err(Error::UnknownRemote(name.to_string()));
        }
    }

    std::fs::create_dir_all(&f.local).map_err(|e| Error::io(f.local.display(), e))?;
    // bisync aborts with `directory not found` rather than creating the remote path, so
    // a first init against a folder that does not exist on the remote yet has to make it.
    session.rclone.mkdir(&f.remote)?;
    let workdir = paths::bisync_workdir(&f.name);
    std::fs::create_dir_all(&workdir).map_err(|e| Error::io(workdir.display(), e))?;

    let local_path = f.local.display().to_string();
    println!(
        "{}: establishing baseline ({} <-> {})",
        f.name, local_path, f.remote
    );

    session.rclone.bisync(
        &local_path,
        &f.remote,
        &workdir.display().to_string(),
        &["--resync"],
    )?;

    // After a successful resync both sides agree, so either listing is a valid merge
    // base. Read the local one: it is cheap and cannot be affected by remote latency.
    let entries = session.rclone.lsjson(&local_path)?;
    let count = entries.len();
    Snapshot::new(&f.name, &session.machine_id, entries).save()?;

    println!("{}: baseline recorded, {count} file(s)", f.name);
    Ok(())
}
