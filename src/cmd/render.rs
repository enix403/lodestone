//! Shared plan rendering, used by `status` and by the mutating commands so the preview a
//! user sees is produced by the same code as the thing that gates the mutation.

use lodestone::error::ExitCode;
use lodestone::plan::{Conflict, ConflictKind, Plan, SideDelta};

pub fn render_plan(plan: &Plan, gate: Option<&lodestone::Error>) {
    println!("{}  {}", plan.folder, plan.summary());
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

pub fn render_side(label: &str, d: &SideDelta) {
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

pub fn describe_conflict(k: &ConflictKind) -> &'static str {
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

pub fn side_json(d: &SideDelta) -> serde_json::Value {
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

pub fn conflict_json(c: &Conflict) -> serde_json::Value {
    serde_json::json!({ "path": c.path, "kind": describe_conflict(&c.kind) })
}

pub fn plan_json(plan: &Plan) -> serde_json::Value {
    serde_json::json!({
        "summary": plan.summary(),
        "local": side_json(&plan.local),
        "remote": side_json(&plan.remote),
        "conflicts": plan.conflicts.iter().map(conflict_json).collect::<Vec<_>>(),
    })
}

/// Higher exit codes are more specific failures; keep the most informative one.
pub fn worse(a: ExitCode, b: ExitCode) -> ExitCode {
    if b.as_i32() > a.as_i32() {
        b
    } else {
        a
    }
}
