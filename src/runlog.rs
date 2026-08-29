//! Run history.
//!
//! There is no daemon whose logs you would read after the fact, so lodestone is
//! interactive-first: the foreground shows a clean summary, and rclone's raw output goes to
//! a file you can reach for when something looks wrong.
//!
//! Storage is append-only JSONL rather than the SQLite the design originally called for.
//! At roughly thirty mutating runs a month there is nothing to index, an append is
//! atomic enough to survive a kill mid-write (a torn final line is discarded on read), and
//! it costs no dependency and stays greppable.

use crate::error::{Error, Result};
use crate::{paths, timestamp};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::PathBuf;

/// Raw logs kept per folder before the oldest are dropped.
pub const KEEP_LOGS: usize = 50;
/// Records kept in the history file; trimmed to half this when exceeded.
const MAX_RECORDS: usize = 1000;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Record {
    pub id: String,
    /// Unix seconds, for rendering and ordering.
    pub at: u64,
    pub folder: String,
    /// `sync`, `push`, `pull`, `init`, `resync`.
    pub command: String,
    /// `applied`, `clean`, `skipped`, `failed`.
    pub outcome: String,
    pub exit_code: i32,
    pub summary: String,
    #[serde(default)]
    pub moved: usize,
    #[serde(default)]
    pub transferred: usize,
    #[serde(default)]
    pub trashed: usize,
    #[serde(default)]
    pub duration_ms: u64,
    /// Why it was skipped or how it failed.
    #[serde(default)]
    pub detail: Option<String>,
    /// Set when raw rclone output was captured.
    #[serde(default)]
    pub has_log: bool,
}

pub fn history_path() -> PathBuf {
    paths::state_dir().join("runs.jsonl")
}

pub fn log_dir(folder: &str) -> PathBuf {
    paths::state_dir().join("logs").join(folder)
}

pub fn log_path(folder: &str, id: &str) -> PathBuf {
    log_dir(folder).join(format!("{id}.log"))
}

/// A run id that does not collide with one already recorded for this folder.
///
/// Two runs can start in the same second when several folders are fanned out, so a
/// suffix is appended rather than letting one overwrite the other's log.
pub fn new_id(folder: &str, at: u64) -> String {
    let base = timestamp::format_compact(at);
    if !log_path(folder, &base).exists() {
        return base;
    }
    (2..)
        .map(|n| format!("{base}-{n}"))
        .find(|id| !log_path(folder, id).exists())
        .unwrap_or(base)
}

/// Store rclone's raw output for a run, and drop the oldest logs.
pub fn write_log(folder: &str, id: &str, contents: &str) -> Result<()> {
    let dir = log_dir(folder);
    std::fs::create_dir_all(&dir).map_err(|e| Error::io(dir.display(), e))?;
    let path = log_path(folder, id);
    std::fs::write(&path, contents).map_err(|e| Error::io(path.display(), e))?;
    prune_logs(folder, KEEP_LOGS)
}

pub fn read_log(folder: &str, id: &str) -> Option<String> {
    std::fs::read_to_string(log_path(folder, id)).ok()
}

fn prune_logs(folder: &str, keep: usize) -> Result<()> {
    let dir = log_dir(folder);
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return Ok(());
    };
    let mut names: Vec<String> = rd
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "log"))
        .filter_map(|e| e.file_name().to_str().map(str::to_string))
        .collect();
    if names.len() <= keep {
        return Ok(());
    }
    // Ids lead with a sortable timestamp, so name order is age order.
    names.sort();
    for name in names.iter().take(names.len() - keep) {
        let p = dir.join(name);
        std::fs::remove_file(&p).map_err(|e| Error::io(p.display(), e))?;
    }
    Ok(())
}

pub fn append(record: &Record) -> Result<()> {
    let path = history_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| Error::io(parent.display(), e))?;
    }
    let line = serde_json::to_string(record)?;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|e| Error::io(path.display(), e))?;
    writeln!(f, "{line}").map_err(|e| Error::io(path.display(), e))?;
    drop(f);
    trim_history()
}

/// Keep the history file from growing without bound.
fn trim_history() -> Result<()> {
    let path = history_path();
    let Ok(body) = std::fs::read_to_string(&path) else {
        return Ok(());
    };
    let lines: Vec<&str> = body.lines().collect();
    if lines.len() <= MAX_RECORDS {
        return Ok(());
    }
    let keep = &lines[lines.len() - MAX_RECORDS / 2..];
    let tmp = path.with_extension("jsonl.tmp");
    std::fs::write(&tmp, format!("{}\n", keep.join("\n")))
        .map_err(|e| Error::io(tmp.display(), e))?;
    std::fs::rename(&tmp, &path).map_err(|e| Error::io(path.display(), e))
}

/// Recorded runs, newest first.
///
/// Malformed lines are skipped rather than failing the read: a torn final line from a
/// killed process must not make the whole history unreadable.
pub fn list(folder: Option<&str>, limit: usize) -> Vec<Record> {
    let Ok(body) = std::fs::read_to_string(history_path()) else {
        return Vec::new();
    };
    let mut out: Vec<Record> = body
        .lines()
        .filter_map(|l| serde_json::from_str::<Record>(l).ok())
        .filter(|r| folder.is_none_or(|f| r.folder == f))
        .collect();
    out.reverse();
    out.truncate(limit);
    out
}

/// Find a run by id, optionally scoped to a folder.
pub fn find(id: &str, folder: Option<&str>) -> Option<Record> {
    list(folder, usize::MAX).into_iter().find(|r| r.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_state<T>(f: impl FnOnce() -> T) -> T {
        let _g = crate::testlock::env_lock();
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("XDG_STATE_HOME", tmp.path());
        let out = f();
        std::env::remove_var("XDG_STATE_HOME");
        out
    }

    fn rec(id: &str, folder: &str, outcome: &str, at: u64) -> Record {
        Record {
            id: id.into(),
            at,
            folder: folder.into(),
            command: "push".into(),
            outcome: outcome.into(),
            exit_code: 0,
            summary: "↑ 1 outgoing".into(),
            moved: 0,
            transferred: 1,
            trashed: 0,
            duration_ms: 1200,
            detail: None,
            has_log: false,
        }
    }

    #[test]
    fn records_come_back_newest_first() {
        with_state(|| {
            append(&rec("a", "docs", "applied", 100)).unwrap();
            append(&rec("b", "docs", "applied", 200)).unwrap();
            append(&rec("c", "notes", "failed", 300)).unwrap();

            let all = list(None, 10);
            assert_eq!(
                all.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
                ["c", "b", "a"]
            );
            assert_eq!(list(Some("docs"), 10).len(), 2);
            assert_eq!(list(None, 2).len(), 2, "limit applies");
        });
    }

    #[test]
    fn a_torn_line_does_not_break_the_history() {
        with_state(|| {
            append(&rec("a", "docs", "applied", 100)).unwrap();
            // Simulate a process killed mid-write.
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(history_path())
                .unwrap();
            write!(f, "{{\"id\":\"partial\",\"fol").unwrap();
            drop(f);

            let all = list(None, 10);
            assert_eq!(all.len(), 1, "the intact record must still be readable");
            assert_eq!(all[0].id, "a");
        });
    }

    #[test]
    fn ids_do_not_collide_within_one_second() {
        with_state(|| {
            let at = 1_788_000_000;
            let first = new_id("docs", at);
            write_log("docs", &first, "log one").unwrap();
            let second = new_id("docs", at);
            assert_ne!(first, second, "a fanned-out run must not overwrite another");
            write_log("docs", &second, "log two").unwrap();
            assert_eq!(read_log("docs", &first).unwrap(), "log one");
            assert_eq!(read_log("docs", &second).unwrap(), "log two");
        });
    }

    #[test]
    fn old_logs_are_pruned_oldest_first() {
        with_state(|| {
            for i in 0..(KEEP_LOGS + 5) {
                let id = timestamp::format_compact(1_788_000_000 + i as u64);
                write_log("docs", &id, &format!("run {i}")).unwrap();
            }
            let kept: Vec<_> = std::fs::read_dir(log_dir("docs"))
                .unwrap()
                .flatten()
                .map(|e| e.file_name().to_string_lossy().to_string())
                .collect();
            assert_eq!(kept.len(), KEEP_LOGS);
            // The five oldest are gone, the newest survives.
            let newest = timestamp::format_compact(1_788_000_000 + (KEEP_LOGS + 4) as u64);
            assert!(read_log("docs", &newest).is_some());
            let oldest = timestamp::format_compact(1_788_000_000);
            assert!(read_log("docs", &oldest).is_none());
        });
    }

    #[test]
    fn find_locates_a_run_by_id() {
        with_state(|| {
            append(&rec("x1", "docs", "applied", 100)).unwrap();
            append(&rec("x2", "notes", "applied", 200)).unwrap();
            assert_eq!(find("x2", None).unwrap().folder, "notes");
            assert!(
                find("x2", Some("docs")).is_none(),
                "folder scopes the search"
            );
            assert!(find("nope", None).is_none());
        });
    }

    #[test]
    fn history_is_trimmed_when_it_grows_too_large() {
        with_state(|| {
            for i in 0..(MAX_RECORDS + 10) {
                append(&rec(&format!("r{i}"), "docs", "applied", i as u64)).unwrap();
            }
            let n = std::fs::read_to_string(history_path())
                .unwrap()
                .lines()
                .count();
            assert!(
                n <= MAX_RECORDS,
                "history should have been trimmed, got {n}"
            );
            // Trimming keeps the newest.
            assert_eq!(list(None, 1)[0].id, format!("r{}", MAX_RECORDS + 9));
        });
    }
}
