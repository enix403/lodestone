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
use lodestone::plan::{Direction, Plan};
use lodestone::session::Session;
use lodestone::{Error, Result};

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
        let Ok(plan) = res else {
            results.push((*f, Err(SkipOrFail::Skipped("plan failed".into()))));
            continue;
        };
        if let Some(e) = gate {
            results.push((*f, Err(SkipOrFail::Skipped(first_line(&e.to_string())))));
            continue;
        }
        if plan.is_clean() {
            results.push((*f, Ok(None)));
            continue;
        }
        match session.apply(f) {
            Ok(applied) => results.push((*f, Ok(Some(applied)))),
            Err(e) => {
                worst = worse(worst, e.exit_code());
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
            Err(SkipOrFail::Skipped(why)) => println!("  {:<16} skipped — {why}", f.name),
            Err(SkipOrFail::Failed(e)) => {
                println!("  {:<16} FAILED — {}", f.name, first_line(&e.to_string()))
            }
        }
    }

    Ok(worst)
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
