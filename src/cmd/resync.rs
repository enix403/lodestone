//! `lode resync` — deliberately re-establish a folder's baseline.
//!
//! This is the escape hatch for every state that leaves a folder without a usable merge
//! base: a deleted snapshot, a regenerated machine id, or bisync demanding a new baseline
//! after an interruption.
//!
//! It is deliberately awkward to invoke, because a resync **unions both sides**. Anything
//! deleted locally but never synced comes back from the remote, and an unsynced
//! reorganisation ends up present at *both* the old and new paths. Nothing is lost, but
//! the result can need manual cleanup — so this must be a decision, never a fallback.

use lodestone::config::Config;
use lodestone::error::ExitCode;
use lodestone::session::Session;
use lodestone::{paths, Error, Result};

pub fn run(cfg: &Config, name: &str, confirmed: bool) -> Result<ExitCode> {
    let f = cfg.get(name)?;
    let session = Session::new()?;

    // Show what a resync would actually do to *these* two sides before doing it. This
    // needs no snapshot, which is the point: it works precisely when `status` cannot.
    let comparison = session.compare(f)?;
    println!("{name}: {}", crate::cmd::compare::summarise(&comparison));
    crate::cmd::compare::render(&comparison);

    if !comparison.differing.is_empty() {
        println!();
        println!(
            "  WARNING: {} file(s) differ on both sides. A resync resolves these in favour",
            comparison.differing.len()
        );
        println!("  of the LOCAL copy, overwriting the remote one. That overwrite is not");
        println!("  captured in lode's trash — only whatever versioning the remote keeps.");
    }

    if !confirmed {
        println!();
        return Err(Error::Config(format!(
            "`lode resync {name}` re-establishes the baseline by unioning both sides.\n\
             Anything you deleted locally but never synced will be restored from the remote,\n\
             and an unsynced reorganisation will leave files at BOTH the old and new paths.\n\
             Nothing is deleted, but you may have cleanup to do.\n\
             Re-run with --i-understand once you have checked the comparison above."
        )));
    }

    let remotes = session.rclone.list_remotes()?;

    // Drop the stale snapshot first so the baseline is rebuilt from what is actually
    // there, rather than compared against a merge base we have already decided to distrust.
    let snapshot = paths::snapshot_path(&f.name);
    if snapshot.exists() {
        std::fs::remove_file(&snapshot).map_err(|e| Error::io(snapshot.display(), e))?;
    }

    println!("{name}: re-establishing baseline (union of both sides)");
    crate::cmd::init::establish_baseline(&session, &remotes, f)?;
    println!("{name}: run `lode status {name}` and check the result");
    Ok(ExitCode::Ok)
}
