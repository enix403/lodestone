//! `lode trash list|restore|prune`.
//!
//! The safety net for the local side. Every apply routes locally-removed and
//! locally-overwritten files into a timestamped run directory instead of destroying them.

use lodestone::config::Config;
use lodestone::error::ExitCode;
use lodestone::{timestamp, trash, Result};

const DEFAULT_MAX_AGE_DAYS: u64 = 30;

pub fn list(cfg: &Config, target: Option<&str>, json: bool) -> Result<ExitCode> {
    let folders = crate::resolve_targets(cfg, target)?;

    if json {
        let items: Vec<_> = folders
            .iter()
            .flat_map(|f| {
                trash::list(&f.name).into_iter().map(move |e| {
                    serde_json::json!({
                        "folder": f.name,
                        "run": e.run,
                        "path": e.rel,
                        "size": e.size,
                        "deleted_at": timestamp::parse_compact(&e.run).map(timestamp::format_rfc3339),
                    })
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&items)?);
        return Ok(ExitCode::Ok);
    }

    let mut any = false;
    for f in folders {
        let entries = trash::list(&f.name);
        if entries.is_empty() {
            continue;
        }
        any = true;
        println!("{}", f.name);
        let mut current = String::new();
        for e in entries {
            if e.run != current {
                let when = timestamp::parse_compact(&e.run)
                    .map(timestamp::format_rfc3339)
                    .unwrap_or_else(|| e.run.clone());
                println!("  {}  ({when})", e.run);
                current = e.run.clone();
            }
            println!("    {:>9}  {}", human_size(e.size), e.rel);
        }
    }
    if !any {
        println!("trash is empty");
    }
    Ok(ExitCode::Ok)
}

pub fn restore(
    cfg: &Config,
    folder: &str,
    path: &str,
    run: Option<&str>,
    overwrite: bool,
) -> Result<ExitCode> {
    let f = cfg.get(folder)?;
    let (from_run, dest) = trash::restore(&f.name, &f.local, path, run, overwrite)?;
    println!("{}: restored {path} from run {from_run}", f.name);
    println!("  -> {}", dest.display());
    // Being explicit matters: the file is now a local change like any other.
    println!(
        "  it is now a local addition — run `lode push {}` to send it to the remote",
        f.name
    );
    println!("  the trashed copy was kept, in case this restore was the mistake");
    Ok(ExitCode::Ok)
}

pub fn prune(
    cfg: &Config,
    target: Option<&str>,
    older_than_days: Option<u64>,
    all: bool,
) -> Result<ExitCode> {
    let folders = crate::resolve_targets(cfg, target)?;
    let max_age = if all {
        None
    } else {
        Some(older_than_days.unwrap_or(DEFAULT_MAX_AGE_DAYS) * 86_400)
    };
    let now = timestamp::now_unix();

    let mut total = 0;
    for f in folders {
        let removed = trash::prune(&f.name, now, max_age)?;
        total += removed.len();
        for run in removed {
            println!("{}: pruned run {run}", f.name);
        }
    }
    if total == 0 {
        match max_age {
            None => println!("nothing to prune"),
            Some(secs) => println!("nothing older than {} day(s) to prune", secs / 86_400),
        }
    }
    Ok(ExitCode::Ok)
}

fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "K", "M", "G", "T"];
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{bytes} {}", UNITS[0])
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

#[cfg(test)]
mod tests {
    use super::human_size;

    #[test]
    fn sizes_render_readably() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(999), "999 B");
        assert_eq!(human_size(1024), "1.0 K");
        assert_eq!(human_size(1_572_864), "1.5 M");
        assert_eq!(human_size(5_368_709_120), "5.0 G");
    }
}
