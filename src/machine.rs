//! Machine identity.
//!
//! Snapshots are machine-local. If one machine's snapshot were ever copied to another
//! (e.g. by accidentally placing the state directory inside a synced folder), the
//! three-way delta would be computed against the wrong base and could classify a whole
//! tree as deleted. Every snapshot therefore carries the id of the machine that wrote
//! it, and lodestone refuses to use a snapshot stamped with a different id.

use crate::error::{Error, Result};
use crate::paths;
use std::io::Write;

/// Stable per-machine identifier: `<hostname>-<random-hex>`, persisted on first use.
pub fn machine_id() -> Result<String> {
    let path = paths::state_dir().join("machine.id");
    if let Ok(s) = std::fs::read_to_string(&path) {
        let s = s.trim().to_string();
        if !s.is_empty() {
            return Ok(s);
        }
    }
    let id = format!("{}-{}", hostname(), random_hex());
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| Error::io(parent.display(), e))?;
    }
    let mut f = std::fs::File::create(&path).map_err(|e| Error::io(path.display(), e))?;
    writeln!(f, "{id}").map_err(|e| Error::io(path.display(), e))?;
    Ok(id)
}

pub fn hostname() -> String {
    // Prefer the syscall via `hostname(1)`; fall back to env, then a constant. The id is
    // only ever compared for equality against itself, so a fallback is harmless.
    if let Ok(out) = std::process::Command::new("hostname").output() {
        if out.status.success() {
            let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !s.is_empty() {
                return sanitise(&s);
            }
        }
    }
    std::env::var("HOSTNAME")
        .ok()
        .filter(|s| !s.is_empty())
        .map(|s| sanitise(&s))
        .unwrap_or_else(|| "unknown".into())
}

fn sanitise(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

fn random_hex() -> String {
    // No RNG dependency: mix the wall clock with the pid. This runs once per machine and
    // only needs to avoid collisions between a handful of hosts.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mixed = (nanos as u64) ^ ((std::process::id() as u64) << 32);
    format!("{mixed:016x}")
}
