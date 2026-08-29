//! `lode sync`, `lode push`, `lode pull` — the mutating commands.
//!
//! All three run the same bidirectional bisync; they differ only in the assertion the plan
//! must satisfy first (see [`Direction`]). Naming them `push`/`pull` is honest because the
//! assertion is enforced: `push` means "send my changes up, and confirm nothing is coming
//! down", not "mirror local over remote" — which would delete whatever another machine
//! added.
//!
//! Fan-out is **plan-all-then-apply**: every folder is planned and the combined summary
//! printed before anything mutates, so a surprise in the last folder is visible before the
//! first one is touched.

use crate::cmd::render::{plan_json, render_plan, worse};
use lodestone::config::{Config, Folder};
use lodestone::error::ExitCode;
use lodestone::lock::Lock;
use lodestone::plan::{Direction, Plan};
use lodestone::session::Session;
use lodestone::{runlog, timestamp, Error, Result};

pub struct Options {
    pub direction: Direction,
    pub json: bool,
    pub dry_run: bool,
    /// Raises the true-delete ceiling for this run only.
    pub allow_deletes: Option<usize>,
}

pub fn run(cfg: &Config, target: Option<&str>, opts: &Options) -> Result<ExitCode> {
    let folders = crate::resolve_targets(cfg, target)?;
    let session = Session::new()?;

    // --- Plan every folder before applying any of them. ---
    let mut planned: Vec<(&Folder, std::result::Result<Plan, Error>, Option<Error>)> = Vec::new();
    let mut worst = ExitCode::Ok;

    for f in folders {
        match session.plan(f) {
            Ok(plan) => {
                let limit = opts.allow_deletes.unwrap_or(f.max_deletes);
                let gate = plan.evaluate(limit, opts.direction);
                if let Err(e) = &gate {
                    worst = worse(worst, e.exit_code());
                }
                planned.push((f, Ok(plan), gate.err()));
            }
            Err(e) => {
                worst = worse(worst, e.exit_code());
                planned.push((f, Err(e), None));
            }
        }
    }

    if !opts.json {
        for (i, (f, res, gate)) in planned.iter().enumerate() {
            if i > 0 {
                println!();
            }
            match res {
                Err(e) => println!("{}\n  error: {e}", f.name),
                Ok(plan) => render_plan(plan, gate.as_ref()),
            }
        }
    }

    if opts.dry_run {
        if opts.json {
            println!("{}", serde_json::to_string_pretty(&dry_run_json(&planned))?);
        } else {
            println!("\n(dry run — nothing was changed)");
        }
        return Ok(worst);
    }

    // --- Apply only the folders whose plan is clean. ---
    let mut results = Vec::new();
    for (f, res, gate) in &planned {
        let summary = res.as_ref().map(|p| p.summary()).unwrap_or_default();
        let started = std::time::Instant::now();

        let Ok(plan) = res else {
            let why = "plan failed".to_string();
            record(f, opts, "skipped", &summary, &why, started, None);
            results.push((*f, Err(SkipOrFail::Skipped(why))));
            continue;
        };
        if let Some(e) = gate {
            let why = first_line(&e.to_string());
            record(f, opts, "skipped", &summary, &why, started, None);
            results.push((*f, Err(SkipOrFail::Skipped(why))));
            continue;
        }
        if plan.is_clean() {
            record(f, opts, "clean", &summary, "", started, None);
            results.push((*f, Ok(None)));
            continue;
        }
        // Held only across the mutation. Taking it earlier would block a concurrent
        // read-only plan for no benefit; the guard releases on drop either way.
        let _lock = match Lock::acquire(&f.name) {
            Ok(l) => l,
            Err(e) => {
                worst = worse(worst, e.exit_code());
                record(f, opts, "skipped", &summary, &e.to_string(), started, None);
                // Full message, not just the first line: unlike a gate failure this was
                // never printed during the plan phase, and the advice it carries — that
                // the other run is alive and its lock must NOT be cleared — is the whole
                // point of the message.
                results.push((*f, Err(SkipOrFail::Skipped(e.to_string()))));
                continue;
            }
        };
        match session.apply(f) {
            Ok(applied) => {
                record(f, opts, "applied", &summary, "", started, Some(&applied));
                results.push((*f, Ok(Some(applied))));
            }
            Err(e) => {
                worst = worse(worst, e.exit_code());
                record(f, opts, "failed", &summary, &e.to_string(), started, None);
                results.push((*f, Err(SkipOrFail::Failed(e))));
            }
        }
    }

    if opts.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&apply_json(&planned, &results))?
        );
        return Ok(worst);
    }

    println!("\nresult");
    for (f, r) in &results {
        match r {
            Ok(None) => println!("  {:<16} already up to date", f.name),
            Ok(Some(a)) => {
                println!(
                    "  {:<16} ok — {} moved server-side, {} transferred, {} file(s) total",
                    f.name, a.moved, a.copied, a.files
                );
                if let Some(run) = &a.trash_run {
                    println!(
                        "  {:<16} {} local file(s) moved to trash, run {run} — `lode trash list {}`",
                        "", a.trashed, f.name
                    );
                }
                // Should be unreachable: the plan aborts on conflicts. If it fires, the
                // plan→apply window was hit and the user must look.
                for p in &a.conflict_artifacts {
                    println!("  {:<16} ! conflict file created: {p}", "");
                    worst = worse(worst, ExitCode::Conflict);
                }
            }
            Err(SkipOrFail::Skipped(why)) => {
                let mut lines = why.lines();
                println!(
                    "  {:<16} skipped — {}",
                    f.name,
                    lines.next().unwrap_or_default()
                );
                for line in lines {
                    println!("  {:<16}   {line}", "");
                }
            }
            Err(SkipOrFail::Failed(e)) => {
                println!("  {:<16} FAILED — {}", f.name, first_line(&e.to_string()))
            }
        }
    }

    Ok(worst)
}

/// Append a run to the history, storing rclone's raw output when there was any.
///
/// Never fails the command: losing a history entry is annoying, losing the sync result
/// because the history could not be written would be absurd.
fn record(
    f: &Folder,
    opts: &Options,
    outcome: &str,
    summary: &str,
    detail: &str,
    started: std::time::Instant,
    applied: Option<&lodestone::session::Applied>,
) {
    let at = timestamp::now_unix();
    let id = runlog::new_id(&f.name, at);
    let has_log = match applied {
        Some(a) => runlog::write_log(&f.name, &id, &a.log).is_ok(),
        None => false,
    };
    let rec = runlog::Record {
        id,
        at,
        folder: f.name.clone(),
        command: match opts.direction {
            Direction::Both => "sync",
            Direction::Push => "push",
            Direction::Pull => "pull",
        }
        .to_string(),
        outcome: outcome.to_string(),
        exit_code: 0,
        summary: summary.to_string(),
        moved: applied.map(|a| a.moved).unwrap_or(0),
        transferred: applied.map(|a| a.copied).unwrap_or(0),
        trashed: applied.map(|a| a.trashed).unwrap_or(0),
        duration_ms: started.elapsed().as_millis() as u64,
        detail: (!detail.is_empty()).then(|| first_line(detail)),
        has_log,
    };
    let _ = runlog::append(&rec);
}

enum SkipOrFail {
    Skipped(String),
    Failed(Error),
}

fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or_default().to_string()
}

fn dry_run_json(
    planned: &[(&Folder, std::result::Result<Plan, Error>, Option<Error>)],
) -> serde_json::Value {
    serde_json::Value::Array(
        planned
            .iter()
            .map(|(f, res, gate)| match res {
                Ok(plan) => {
                    let mut v = plan_json(plan);
                    v["folder"] = serde_json::Value::String(f.name.clone());
                    v["ok"] = serde_json::Value::Bool(true);
                    v["dry_run"] = serde_json::Value::Bool(true);
                    v["blocked"] = match gate {
                        Some(e) => serde_json::Value::String(e.to_string()),
                        None => serde_json::Value::Null,
                    };
                    v
                }
                Err(e) => serde_json::json!({
                    "folder": f.name, "ok": false, "error": e.to_string()
                }),
            })
            .collect(),
    )
}

fn apply_json(
    planned: &[(&Folder, std::result::Result<Plan, Error>, Option<Error>)],
    results: &[(
        &Folder,
        std::result::Result<Option<lodestone::session::Applied>, SkipOrFail>,
    )],
) -> serde_json::Value {
    serde_json::Value::Array(
        results
            .iter()
            .map(|(f, r)| {
                let plan = planned
                    .iter()
                    .find(|(pf, _, _)| pf.name == f.name)
                    .and_then(|(_, res, _)| res.as_ref().ok())
                    .map(plan_json)
                    .unwrap_or(serde_json::Value::Null);
                match r {
                    Ok(None) => serde_json::json!({
                        "folder": f.name, "ok": true, "applied": false,
                        "reason": "already up to date", "plan": plan
                    }),
                    Ok(Some(a)) => serde_json::json!({
                        "folder": f.name, "ok": true, "applied": true,
                        "moved_server_side": a.moved, "transferred": a.copied,
                        "files": a.files, "conflict_artifacts": a.conflict_artifacts,
                        "trash_run": a.trash_run, "trashed": a.trashed,
                        "plan": plan
                    }),
                    Err(SkipOrFail::Skipped(why)) => serde_json::json!({
                        "folder": f.name, "ok": false, "applied": false,
                        "skipped": why, "plan": plan
                    }),
                    Err(SkipOrFail::Failed(e)) => serde_json::json!({
                        "folder": f.name, "ok": false, "applied": false,
                        "error": e.to_string(), "plan": plan
                    }),
                }
            })
            .collect(),
    )
}
