//! `lode compare` — a two-way diff of the sides, needing no snapshot.
//!
//! `status` answers "what happened since the last sync", which requires the merge base.
//! When the snapshot is gone that question has no answer — but "how do the two sides
//! differ right now?" still does, and it is exactly what you need before deciding whether
//! re-baselining is safe.

use lodestone::config::Config;
use lodestone::error::ExitCode;
use lodestone::plan::Comparison;
use lodestone::session::Session;
use lodestone::Result;

pub fn run(cfg: &Config, target: Option<&str>, json: bool) -> Result<ExitCode> {
    let folders = crate::resolve_targets(cfg, target)?;
    let session = Session::new()?;

    let mut results = Vec::new();
    for f in folders {
        results.push((f, session.compare(f)));
    }

    if json {
        let items: Vec<_> = results
            .iter()
            .map(|(f, r)| match r {
                Ok(c) => serde_json::json!({
                    "folder": f.name, "ok": true,
                    "local_only": c.local_only, "remote_only": c.remote_only,
                    "differing": c.differing, "identical": c.identical,
                    "in_sync": c.in_sync(),
                }),
                Err(e) => serde_json::json!({
                    "folder": f.name, "ok": false, "error": e.to_string()
                }),
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&items)?);
        return Ok(ExitCode::Ok);
    }

    for (i, (f, r)) in results.iter().enumerate() {
        if i > 0 {
            println!();
        }
        match r {
            Err(e) => println!("{}\n  error: {e}", f.name),
            Ok(c) => {
                println!("{}  {}", f.name, summarise(c));
                render(c);
            }
        }
    }
    Ok(ExitCode::Ok)
}

pub fn summarise(c: &Comparison) -> String {
    if c.in_sync() {
        return format!("both sides identical ({} file(s))", c.identical);
    }
    let mut parts = Vec::new();
    if !c.local_only.is_empty() {
        parts.push(format!("{} only local", c.local_only.len()));
    }
    if !c.remote_only.is_empty() {
        parts.push(format!("{} only remote", c.remote_only.len()));
    }
    if !c.differing.is_empty() {
        parts.push(format!("{} differ", c.differing.len()));
    }
    format!("{} ({} identical)", parts.join(", "), c.identical)
}

pub fn render(c: &Comparison) {
    for p in &c.local_only {
        println!("  local only   {p}");
    }
    for p in &c.remote_only {
        println!("  remote only  {p}");
    }
    for p in &c.differing {
        println!("  differ       {p}");
    }
}
