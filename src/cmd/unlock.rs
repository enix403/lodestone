//! `lode unlock` — clear a bisync lock left behind by an interrupted run.
//!
//! Deliberately manual. bisync's lock exists precisely to stop a second run from
//! compounding an interrupted one, so clearing it must be a decision the user makes after
//! confirming nothing else is running.

use lodestone::config::Config;
use lodestone::error::ExitCode;
use lodestone::lock;
use lodestone::session::clear_lock;
use lodestone::Result;

pub fn run(cfg: &Config, target: Option<&str>) -> Result<ExitCode> {
    for f in crate::resolve_targets(cfg, target)? {
        // Report who held it before clearing. `unlock` is the deliberate escape hatch,
        // so it does clear a live holder — but you should be told that is what happened.
        if let Some(h) = lock::holder(&f.name) {
            println!(
                "{}: lock held by pid {} on {} since {}",
                f.name,
                h.pid,
                h.host,
                lodestone::timestamp::format_rfc3339(h.at)
            );
        }
        let mut cleared = lock::clear(&f.name)?;
        let removed = clear_lock(&f.name)?;
        for p in &removed {
            println!("{}: cleared {p}", f.name);
            cleared = true;
        }
        if cleared {
            println!("{}: lock cleared", f.name);
        } else {
            println!("{}: no lock held", f.name);
        }
    }
    Ok(ExitCode::Ok)
}
