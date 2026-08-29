//! `lode status` — the plan phase, run on its own.
//!
//! Read-only: it lists both sides and computes the plan, but changes nothing. The exit
//! code still reflects the worst gate that *would* have tripped, so it is usable in
//! scripts and as a pre-flight check.

use crate::cmd::render::{plan_json, render_plan, worse};
use lodestone::config::Config;
use lodestone::error::ExitCode;
use lodestone::plan::Direction;
use lodestone::session::Session;
use lodestone::Result;

pub fn run(cfg: &Config, target: Option<&str>, json: bool) -> Result<ExitCode> {
    let folders = crate::resolve_targets(cfg, target)?;
    let session = Session::new()?;

    let mut worst = ExitCode::Ok;
    let mut reports = Vec::new();

    for f in folders {
        match session.plan(f) {
            Ok(plan) => {
                // A folder that fails its gate must not silence the others; collect the
                // worst outcome and report everything at the end.
                let gate = plan.evaluate(f.max_deletes, Direction::Both);
                if let Err(e) = &gate {
                    worst = worse(worst, e.exit_code());
                }
                reports.push((f, Ok(plan), gate.err()));
            }
            Err(e) => {
                worst = worse(worst, e.exit_code());
                reports.push((f, Err(e), None));
            }
        }
    }

    if json {
        let items: Vec<_> = reports
            .iter()
            .map(|(f, res, gate)| match res {
                Ok(plan) => {
                    let mut v = plan_json(plan);
                    v["folder"] = serde_json::Value::String(f.name.clone());
                    v["ok"] = serde_json::Value::Bool(true);
                    v["blocked"] = match gate {
                        Some(e) => serde_json::Value::String(e.to_string()),
                        None => serde_json::Value::Null,
                    };
                    v
                }
                Err(e) => serde_json::json!({
                    "folder": f.name,
                    "ok": false,
                    "error": e.to_string(),
                }),
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&items)?);
        return Ok(worst);
    }

    for (i, (f, res, gate)) in reports.iter().enumerate() {
        if i > 0 {
            println!();
        }
        match res {
            Err(e) => println!("{}\n  error: {e}", f.name),
            Ok(plan) => render_plan(plan, gate.as_ref()),
        }
    }

    Ok(worst)
}
