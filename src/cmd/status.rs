//! `lode status` — the plan phase, run on its own.
//!
//! Read-only: it lists both sides and computes the plan, but changes nothing. The exit
//! code still reflects the worst gate that *would* have tripped, so it is usable in
//! scripts and as a pre-flight check.

use lodestone::config::{Config, Folder};
use lodestone::error::ExitCode;
use lodestone::plan::{Conflict, ConflictKind, Direction, Plan, SideDelta};
use lodestone::snapshot::Snapshot;
use lodestone::{machine, rclone::Rclone, Error, Result};

pub fn run(cfg: &Config, target: Option<&str>, json: bool) -> Result<ExitCode> {
    let folders = crate::resolve_targets(cfg, target)?;
    let rclone = Rclone::discover()?;
    rclone.require_min_version()?;
    let machine_id = machine::machine_id()?;

    let mut worst = ExitCode::Ok;
    let mut reports = Vec::new();

    for f in folders {
        match plan_for(&rclone, &machine_id, f) {
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
                Ok(plan) => serde_json::json!({
                    "folder": f.name,
                    "ok": true,
                    "summary": plan.summary(),
                    "local": side_json(&plan.local),
                    "remote": side_json(&plan.remote),
                    "conflicts": plan.conflicts.iter().map(conflict_json).collect::<Vec<_>>(),
                    "blocked": gate.as_ref().map(|e| e.to_string()),
                }),
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
            Ok(plan) => {
                println!("{}  {}", f.name, plan.summary());
                render_side("local", &plan.local);
                render_side("remote", &plan.remote);
                for c in &plan.conflicts {
                    println!("  ! conflict  {}  ({})", c.path, describe_conflict(&c.kind));
                }
                if let Some(e) = gate {
                    println!();
                    for line in e.to_string().lines() {
                        println!("  {line}");
                    }
                }
            }
        }
    }

    Ok(worst)
}

fn plan_for(rclone: &Rclone, machine_id: &str, f: &Folder) -> Result<Plan> {
    let snapshot = Snapshot::load(&f.name, machine_id)?;
    let local_path = f.local.display().to_string();
    if !f.local.exists() {
        return Err(Error::Config(format!(
            "local path {local_path} does not exist"
        )));
    }
    let local = rclone.lsjson(&local_path)?;
    let remote = rclone.lsjson(&f.remote)?;
    Ok(Plan::compute(&f.name, &snapshot.entries, &local, &remote))
}

fn render_side(label: &str, d: &SideDelta) {
    for p in d.added.keys() {
        println!("  {label:<6} +  {p}");
    }
    for p in d.modified.keys() {
        println!("  {label:<6} M  {p}");
    }
    for r in &d.renames {
        println!("  {label:<6} R  {} -> {}", r.from, r.to);
    }
    for p in d.deleted.keys() {
        println!("  {label:<6} -  {p}");
    }
}

fn describe_conflict(k: &ConflictKind) -> &'static str {
    match k {
        ConflictKind::BothEdited => "edited on both sides",
        ConflictKind::BothCreated => "created on both sides with different content",
        ConflictKind::EditedAndDeleted {
            deleted_on_local: true,
        } => "deleted locally, edited on the remote",
        ConflictKind::EditedAndDeleted {
            deleted_on_local: false,
        } => "edited locally, deleted on the remote",
        ConflictKind::Indeterminate => "changed on both sides, content cannot be compared",
    }
}

fn side_json(d: &SideDelta) -> serde_json::Value {
    serde_json::json!({
        "added": d.added.keys().collect::<Vec<_>>(),
        "modified": d.modified.keys().collect::<Vec<_>>(),
        "deleted": d.deleted.keys().collect::<Vec<_>>(),
        "renamed": d.renames.iter()
            .map(|r| serde_json::json!({"from": r.from, "to": r.to}))
            .collect::<Vec<_>>(),
        "true_deletes": d.true_deletes(),
    })
}

fn conflict_json(c: &Conflict) -> serde_json::Value {
    serde_json::json!({ "path": c.path, "kind": describe_conflict(&c.kind) })
}

/// Higher exit codes are more specific failures; keep the most informative one.
fn worse(a: ExitCode, b: ExitCode) -> ExitCode {
    if b.as_i32() > a.as_i32() {
        b
    } else {
        a
    }
}
