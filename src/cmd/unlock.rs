//! `lode unlock` — clear a bisync lock left behind by an interrupted run.
//!
//! Deliberately manual. bisync's lock exists precisely to stop a second run from
//! compounding an interrupted one, so clearing it must be a decision the user makes after
//! confirming nothing else is running.

use lodestone::config::Config;
use lodestone::error::ExitCode;
use lodestone::session::clear_lock;
use lodestone::Result;

pub fn run(cfg: &Config, target: Option<&str>) -> Result<ExitCode> {
    for f in crate::resolve_targets(cfg, target)? {
        let removed = clear_lock(&f.name)?;
        if removed.is_empty() {
            println!("{}: no lock held", f.name);
        } else {
            for p in removed {
                println!("{}: cleared {p}", f.name);
            }
        }
    }
    Ok(ExitCode::Ok)
}
