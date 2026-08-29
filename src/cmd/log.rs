//! `lode log` — what past runs did, and rclone's raw output when you need it.

use lodestone::config::Config;
use lodestone::error::ExitCode;
use lodestone::{runlog, timestamp, Error, Result};

pub fn run(
    cfg: &Config,
    target: Option<&str>,
    limit: usize,
    show: Option<&str>,
    json: bool,
) -> Result<ExitCode> {
    // Validate a named folder so a typo is an error rather than silently empty output.
    let folder = match target {
        Some(".") | None => None,
        Some(name) => Some(cfg.get(name)?.name.as_str()),
    };

    if let Some(id) = show {
        let rec = runlog::find(id, folder)
            .ok_or_else(|| Error::Config(format!("no run with id {id:?}")))?;
        match runlog::read_log(&rec.folder, &rec.id) {
            Some(body) => {
                println!("{}", body.trim_end());
                Ok(ExitCode::Ok)
            }
            None => Err(Error::Config(format!(
                "run {id} has no stored log (it changed nothing, or the log has been rotated out)"
            ))),
        }
    } else {
        list(folder, limit, json)
    }
}

fn list(folder: Option<&str>, limit: usize, json: bool) -> Result<ExitCode> {
    let records = runlog::list(folder, limit);

    if json {
        println!("{}", serde_json::to_string_pretty(&records)?);
        return Ok(ExitCode::Ok);
    }
    if records.is_empty() {
        println!("no runs recorded yet");
        return Ok(ExitCode::Ok);
    }

    for r in &records {
        let when = timestamp::format_rfc3339(r.at);
        let mut line = format!(
            "{}  {when}  {:<8} {:<8} {}",
            r.id, r.command, r.outcome, r.summary
        );
        if r.outcome == "applied" {
            line.push_str(&format!(
                "  [{} moved, {} transferred, {} trashed, {:.1}s]",
                r.moved,
                r.transferred,
                r.trashed,
                r.duration_ms as f64 / 1000.0
            ));
        }
        println!("{line}");
        if let Some(d) = &r.detail {
            println!("    {d}");
        }
        if r.has_log {
            println!("    log: lode log --show {}", r.id);
        }
    }
    Ok(ExitCode::Ok)
}
