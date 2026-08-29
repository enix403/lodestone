//! Local trash.
//!
//! Every apply passes `--backup-dir1` pointing at a fresh, timestamped run directory, so
//! anything bisync would have destroyed on the **local** side — a file deleted on another
//! machine, or a local copy overwritten by an incoming edit — is moved there instead.
//! rclone preserves the relative path, so the trash mirrors the folder's shape:
//!
//! ```text
//! ~/.local/state/lode/trash/silvermine/20260829T191500Z/inbox/doc2.pdf
//! ```
//!
//! There is deliberately **no** `--backup-dir2`. Google Drive already keeps deleted files
//! for 30 days, so a second remote trash would be redundant clutter consuming quota.
//!
//! Only the local side is covered here. That is the side with no other safety net.

use crate::error::{Error, Result};
use crate::{paths, timestamp};
use std::path::{Path, PathBuf};

/// Trash root for a folder: `$XDG_STATE_HOME/lode/trash/<folder>`.
pub fn root(folder: &str) -> PathBuf {
    paths::state_dir().join("trash").join(folder)
}

pub fn run_dir(folder: &str, run: &str) -> PathBuf {
    root(folder).join(run)
}

/// A file sitting in the trash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Run directory name, e.g. `20260829T191500Z`.
    pub run: String,
    /// Path relative to the synced folder — where it came from, and where it goes back to.
    pub rel: String,
    pub size: u64,
}

/// Run directories, oldest first. Anything not named like a run is ignored.
pub fn runs(folder: &str) -> Vec<String> {
    let mut out: Vec<String> = match std::fs::read_dir(root(folder)) {
        Ok(rd) => rd
            .flatten()
            .filter(|e| e.path().is_dir())
            .filter_map(|e| e.file_name().to_str().map(str::to_string))
            .filter(|n| timestamp::parse_compact(n).is_some())
            .collect(),
        Err(_) => Vec::new(),
    };
    // Compact timestamps sort lexicographically into chronological order.
    out.sort();
    out
}

/// Everything in a folder's trash, oldest run first.
pub fn list(folder: &str) -> Vec<Entry> {
    let mut out = Vec::new();
    for run in runs(folder) {
        let base = run_dir(folder, &run);
        let mut files = Vec::new();
        walk(&base, &base, &mut files);
        files.sort_by(|a, b| a.0.cmp(&b.0));
        for (rel, size) in files {
            out.push(Entry {
                run: run.clone(),
                rel,
                size,
            });
        }
    }
    out
}

fn walk(base: &Path, dir: &Path, out: &mut Vec<(String, u64)>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in rd.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk(base, &path, out);
        } else if let Ok(rel) = path.strip_prefix(base) {
            let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
            out.push((rel.to_string_lossy().replace('\\', "/"), size));
        }
    }
}

/// Prepare a run directory for an apply. Returns its path.
pub fn begin_run(folder: &str, at: u64) -> Result<(String, PathBuf)> {
    let run = timestamp::format_compact(at);
    let dir = run_dir(folder, &run);
    std::fs::create_dir_all(&dir).map_err(|e| Error::io(dir.display(), e))?;
    Ok((run, dir))
}

/// Remove a run directory if nothing was placed in it.
///
/// Most runs delete nothing, and a trash full of empty directories would make `trash list`
/// useless noise.
pub fn discard_if_empty(dir: &Path) -> Result<()> {
    if is_empty_tree(dir) {
        std::fs::remove_dir_all(dir).map_err(|e| Error::io(dir.display(), e))?;
    }
    Ok(())
}

fn is_empty_tree(dir: &Path) -> bool {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return false;
    };
    for entry in rd.flatten() {
        let p = entry.path();
        if p.is_dir() {
            if !is_empty_tree(&p) {
                return false;
            }
        } else {
            return false;
        }
    }
    true
}

/// Put a trashed file back into the synced folder.
///
/// Restoring re-creates the file as a *local addition*, so the next sync will propagate it
/// back to the remote. That is the intended behaviour, and callers should say so.
pub fn restore(
    folder: &str,
    local_root: &Path,
    rel: &str,
    run: Option<&str>,
    overwrite: bool,
) -> Result<(String, PathBuf)> {
    // Without an explicit run, take the most recent copy — the one deleted last.
    let candidates: Vec<Entry> = list(folder)
        .into_iter()
        .filter(|e| e.rel == rel)
        .filter(|e| run.is_none_or(|r| e.run == r))
        .collect();

    let chosen = candidates.last().ok_or_else(|| {
        Error::Config(match run {
            Some(r) => format!("{rel:?} is not in trash run {r} for folder {folder:?}"),
            None => format!("{rel:?} is not in the trash for folder {folder:?}"),
        })
    })?;

    let src = run_dir(folder, &chosen.run).join(rel);
    let dest = local_root.join(rel);
    if dest.exists() && !overwrite {
        return Err(Error::Config(format!(
            "{} already exists — pass --overwrite to replace it",
            dest.display()
        )));
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| Error::io(parent.display(), e))?;
    }
    // Copy rather than move: a restore that half-succeeds must not also lose the backup.
    std::fs::copy(&src, &dest).map_err(|e| Error::io(src.display(), e))?;
    Ok((chosen.run.clone(), dest))
}

/// Delete trash runs older than `max_age_secs`, or all of them.
pub fn prune(folder: &str, now: u64, max_age_secs: Option<u64>) -> Result<Vec<String>> {
    let mut removed = Vec::new();
    for run in runs(folder) {
        let keep = match (max_age_secs, timestamp::parse_compact(&run)) {
            (None, _) => false, // prune everything
            (Some(max), Some(at)) => now.saturating_sub(at) <= max,
            // A run whose name will not parse cannot be aged; leave it alone rather than
            // guess. `runs()` already filters these out, so this is belt and braces.
            (Some(_), None) => true,
        };
        if !keep {
            let dir = run_dir(folder, &run);
            std::fs::remove_dir_all(&dir).map_err(|e| Error::io(dir.display(), e))?;
            removed.push(run);
        }
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Point `XDG_STATE_HOME` at a temp dir for the duration of a test.
    fn with_state<T>(f: impl FnOnce() -> T) -> T {
        let _g = crate::testlock::env_lock();
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("XDG_STATE_HOME", tmp.path());
        let out = f();
        std::env::remove_var("XDG_STATE_HOME");
        out
    }

    fn put(folder: &str, run: &str, rel: &str, body: &[u8]) {
        let p = run_dir(folder, run).join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }

    #[test]
    fn lists_entries_oldest_run_first() {
        with_state(|| {
            put("docs", "20240101T000000Z", "inbox/a.pdf", b"old");
            put("docs", "20260101T000000Z", "inbox/b.pdf", b"newer");
            put("docs", "20260101T000000Z", "deep/nested/c.pdf", b"nested");

            let entries = list("docs");
            assert_eq!(entries.len(), 3);
            assert_eq!(entries[0].run, "20240101T000000Z");
            assert_eq!(entries[0].rel, "inbox/a.pdf");
            assert_eq!(entries[0].size, 3);
            // Nested paths are reported relative to the run root.
            assert!(entries.iter().any(|e| e.rel == "deep/nested/c.pdf"));
        });
    }

    #[test]
    fn ignores_directories_that_are_not_runs() {
        with_state(|| {
            put("docs", "20260101T000000Z", "a.pdf", b"x");
            std::fs::create_dir_all(root("docs").join("scratch")).unwrap();
            std::fs::write(root("docs").join("scratch/junk"), b"y").unwrap();

            assert_eq!(runs("docs"), vec!["20260101T000000Z"]);
            assert_eq!(list("docs").len(), 1);
        });
    }

    #[test]
    fn restores_the_most_recent_copy_by_default() {
        with_state(|| {
            let dest = tempfile::tempdir().unwrap();
            put("docs", "20240101T000000Z", "a.pdf", b"older version");
            put("docs", "20260101T000000Z", "a.pdf", b"newer version");

            let (run, path) = restore("docs", dest.path(), "a.pdf", None, false).unwrap();
            assert_eq!(run, "20260101T000000Z");
            assert_eq!(std::fs::read(&path).unwrap(), b"newer version");

            // An explicit run selects the older copy instead.
            let (run, path) =
                restore("docs", dest.path(), "a.pdf", Some("20240101T000000Z"), true).unwrap();
            assert_eq!(run, "20240101T000000Z");
            assert_eq!(std::fs::read(&path).unwrap(), b"older version");
        });
    }

    #[test]
    fn restore_refuses_to_clobber_without_permission() {
        with_state(|| {
            let dest = tempfile::tempdir().unwrap();
            std::fs::write(dest.path().join("a.pdf"), b"current").unwrap();
            put("docs", "20260101T000000Z", "a.pdf", b"trashed");

            let err = restore("docs", dest.path(), "a.pdf", None, false).unwrap_err();
            assert!(err.to_string().contains("--overwrite"), "{err}");
            // The existing file is untouched.
            assert_eq!(
                std::fs::read(dest.path().join("a.pdf")).unwrap(),
                b"current"
            );

            restore("docs", dest.path(), "a.pdf", None, true).unwrap();
            assert_eq!(
                std::fs::read(dest.path().join("a.pdf")).unwrap(),
                b"trashed"
            );
        });
    }

    #[test]
    fn restore_keeps_the_backup() {
        // A restore must not empty the trash: if the restore was a mistake, the backup is
        // still the only copy of the old content.
        with_state(|| {
            let dest = tempfile::tempdir().unwrap();
            put("docs", "20260101T000000Z", "a.pdf", b"trashed");
            restore("docs", dest.path(), "a.pdf", None, false).unwrap();
            assert_eq!(list("docs").len(), 1);
        });
    }

    #[test]
    fn restore_reports_a_missing_file_clearly() {
        with_state(|| {
            let dest = tempfile::tempdir().unwrap();
            let err = restore("docs", dest.path(), "nope.pdf", None, false).unwrap_err();
            assert!(err.to_string().contains("not in the trash"), "{err}");
        });
    }

    #[test]
    fn prune_respects_age_and_can_take_everything() {
        with_state(|| {
            let now = 1_788_000_000u64;
            let day = 86_400u64;
            put(
                "docs",
                &timestamp::format_compact(now - 40 * day),
                "old.pdf",
                b"o",
            );
            put(
                "docs",
                &timestamp::format_compact(now - 5 * day),
                "recent.pdf",
                b"r",
            );

            let removed = prune("docs", now, Some(30 * day)).unwrap();
            assert_eq!(removed.len(), 1);
            assert_eq!(list("docs").len(), 1);
            assert_eq!(list("docs")[0].rel, "recent.pdf");

            assert_eq!(prune("docs", now, None).unwrap().len(), 1);
            assert!(list("docs").is_empty());
        });
    }

    #[test]
    fn empty_runs_are_discarded() {
        with_state(|| {
            let (_run, dir) = begin_run("docs", 1_788_000_000).unwrap();
            // Nested-but-empty must still count as empty; rclone can leave bare dirs.
            std::fs::create_dir_all(dir.join("inbox")).unwrap();
            discard_if_empty(&dir).unwrap();
            assert!(!dir.exists());
            assert!(runs("docs").is_empty());
        });
    }

    #[test]
    fn runs_holding_files_are_kept() {
        with_state(|| {
            let (_run, dir) = begin_run("docs", 1_788_000_000).unwrap();
            std::fs::write(dir.join("kept.pdf"), b"x").unwrap();
            discard_if_empty(&dir).unwrap();
            assert!(dir.exists());
        });
    }
}
