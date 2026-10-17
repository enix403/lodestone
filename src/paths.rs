//! XDG path resolution.
//!
//! lodestone deliberately uses XDG conventions on **both** macOS and Linux rather than
//! `~/Library/Application Support` on macOS. The config file is expected to live in a
//! cross-platform dotfiles repo, so a single uniform path keeps one symlink working
//! everywhere.

use std::path::{Path, PathBuf};

pub fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

fn xdg(var: &str, default_suffix: &str) -> PathBuf {
    match std::env::var_os(var) {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => home().join(default_suffix),
    }
}

/// `$XDG_CONFIG_HOME/lode` (default `~/.config/lode`).
pub fn config_dir() -> PathBuf {
    xdg("XDG_CONFIG_HOME", ".config").join("lode")
}

/// `$XDG_STATE_HOME/lode` (default `~/.local/state/lode`).
///
/// Holds snapshots, run logs, locks and the machine id. Never synced.
pub fn state_dir() -> PathBuf {
    xdg("XDG_STATE_HOME", ".local/state").join("lode")
}

/// `$XDG_CACHE_HOME/lode` (default `~/.cache/lode`).
///
/// Holds only `rename-test` scratch space, so emptying it is always safe.
pub fn cache_dir() -> PathBuf {
    xdg("XDG_CACHE_HOME", ".cache").join("lode")
}

pub fn folder_state_dir(folder: &str) -> PathBuf {
    state_dir().join("folders").join(folder)
}

pub fn snapshot_path(folder: &str) -> PathBuf {
    folder_state_dir(folder).join("snapshot.json")
}

/// bisync's own listings and locks. These are half of the baseline — losing them makes
/// bisync demand a resync — so they live next to the snapshot, not in the cache.
pub fn bisync_workdir(folder: &str) -> PathBuf {
    folder_state_dir(folder).join("bisync")
}

/// Where the bisync workdir lived before it moved into state.
pub fn legacy_bisync_workdir(folder: &str) -> PathBuf {
    cache_dir().join("bisync").join(folder)
}

/// Move a folder's bisync workdir out of its old cache location, if it is still there.
///
/// Idempotent; runs for every configured folder whenever the config is loaded. If both
/// locations exist the new one wins and the old one is left untouched.
pub fn migrate_bisync_workdir(folder: &str) -> crate::Result<()> {
    use crate::Error;

    let old = legacy_bisync_workdir(folder);
    let new = bisync_workdir(folder);
    if !old.is_dir() || new.exists() {
        return Ok(());
    }
    let parent = folder_state_dir(folder);
    std::fs::create_dir_all(&parent).map_err(|e| Error::io(parent.display(), e))?;
    if std::fs::rename(&old, &new).is_ok() {
        return Ok(());
    }

    // State and cache can be on different filesystems when the XDG variables point
    // apart. Copy into a sibling first so an interrupted copy never looks like a
    // complete workdir, then swap it into place.
    let tmp = parent.join("bisync.migrating");
    if tmp.exists() {
        std::fs::remove_dir_all(&tmp).map_err(|e| Error::io(tmp.display(), e))?;
    }
    copy_dir(&old, &tmp)?;
    std::fs::rename(&tmp, &new).map_err(|e| Error::io(new.display(), e))?;
    std::fs::remove_dir_all(&old).map_err(|e| Error::io(old.display(), e))
}

/// Directory holding the filter file.
///
/// bisync stores a hash of the filter file beside it (`filters.txt.md5`) and demands a
/// resync when that hash is missing, so the filter file belongs in state too.
pub fn filters_dir() -> PathBuf {
    state_dir()
}

/// Move the filter hash out of the old cache location, if it is still there. The filter
/// file itself is rewritten every run; only its `.md5` sibling carries bisync state.
pub fn migrate_filters_hash() -> crate::Result<()> {
    use crate::Error;

    let old = cache_dir().join("filters.txt.md5");
    let new = filters_dir().join("filters.txt.md5");
    if !old.is_file() || new.exists() {
        return Ok(());
    }
    let dir = filters_dir();
    std::fs::create_dir_all(&dir).map_err(|e| Error::io(dir.display(), e))?;
    if std::fs::rename(&old, &new).is_ok() {
        return Ok(());
    }
    std::fs::copy(&old, &new).map_err(|e| Error::io(new.display(), e))?;
    std::fs::remove_file(&old).map_err(|e| Error::io(old.display(), e))
}

fn copy_dir(from: &Path, to: &Path) -> crate::Result<()> {
    use crate::Error;

    std::fs::create_dir_all(to).map_err(|e| Error::io(to.display(), e))?;
    let entries = std::fs::read_dir(from).map_err(|e| Error::io(from.display(), e))?;
    for entry in entries {
        let entry = entry.map_err(|e| Error::io(from.display(), e))?;
        let src = entry.path();
        let dst = to.join(entry.file_name());
        if src.is_dir() {
            copy_dir(&src, &dst)?;
        } else {
            std::fs::copy(&src, &dst).map_err(|e| Error::io(src.display(), e))?;
        }
    }
    Ok(())
}

/// Expand a leading `~` against `$HOME`. Only a leading `~/` (or bare `~`) is expanded;
/// `~user` is deliberately unsupported.
pub fn expand_tilde(p: &str) -> PathBuf {
    if p == "~" {
        return home();
    }
    match p.strip_prefix("~/") {
        Some(rest) => home().join(rest),
        None => PathBuf::from(p),
    }
}

/// True if `inner` is `outer` or lies beneath it. Used to enforce that lodestone's own
/// state never lives inside a synced folder — which would sync one machine's snapshot
/// to another and produce catastrophically wrong rename/delete classification.
pub fn is_inside(inner: &Path, outer: &Path) -> bool {
    let inner = normalise(inner);
    let outer = normalise(outer);
    inner.starts_with(&outer)
}

/// Lexical normalisation: resolve `.` and `..` without touching the filesystem, so the
/// check works for paths that do not exist yet.
fn normalise(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tilde_expansion() {
        let _g = crate::testlock::env_lock();
        std::env::set_var("HOME", "/home/alice");
        assert_eq!(
            expand_tilde("~/silvermine"),
            PathBuf::from("/home/alice/silvermine")
        );
        assert_eq!(expand_tilde("~"), PathBuf::from("/home/alice"));
        assert_eq!(expand_tilde("/mnt/data"), PathBuf::from("/mnt/data"));
        // ~user is not expanded, by design.
        assert_eq!(expand_tilde("~bob/x"), PathBuf::from("~bob/x"));
    }

    #[test]
    fn inside_detection() {
        assert!(is_inside(
            Path::new("/home/a/silvermine/.local/state"),
            Path::new("/home/a/silvermine")
        ));
        assert!(is_inside(Path::new("/home/a/x"), Path::new("/home/a/x")));
        assert!(!is_inside(
            Path::new("/home/a/other"),
            Path::new("/home/a/x")
        ));
        // Must not be fooled by a shared path *prefix* that is not a path *component*.
        assert!(!is_inside(
            Path::new("/home/a/silvermine-backup"),
            Path::new("/home/a/silvermine")
        ));
        // Lexical normalisation of `..`.
        assert!(!is_inside(
            Path::new("/home/a/silvermine/../elsewhere"),
            Path::new("/home/a/silvermine")
        ));
    }

    fn with_xdg<T>(f: impl FnOnce() -> T) -> T {
        let _g = crate::testlock::env_lock();
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("XDG_STATE_HOME", tmp.path().join("state"));
        std::env::set_var("XDG_CACHE_HOME", tmp.path().join("cache"));
        let out = f();
        std::env::remove_var("XDG_STATE_HOME");
        std::env::remove_var("XDG_CACHE_HOME");
        out
    }

    #[test]
    fn bisync_workdir_lives_in_state_not_cache() {
        with_xdg(|| {
            assert!(bisync_workdir("docs").starts_with(state_dir()));
            assert!(!bisync_workdir("docs").starts_with(cache_dir()));
        });
    }

    #[test]
    fn legacy_workdir_is_migrated_into_state() {
        with_xdg(|| {
            let old = legacy_bisync_workdir("docs");
            std::fs::create_dir_all(&old).unwrap();
            std::fs::write(old.join("a_b.path1.lst"), "listing").unwrap();

            migrate_bisync_workdir("docs").unwrap();

            let moved = bisync_workdir("docs").join("a_b.path1.lst");
            assert_eq!(std::fs::read_to_string(moved).unwrap(), "listing");
            assert!(!old.exists());
            // Running it again is a no-op.
            migrate_bisync_workdir("docs").unwrap();
        });
    }

    #[test]
    fn migration_never_overwrites_an_existing_workdir() {
        with_xdg(|| {
            let old = legacy_bisync_workdir("docs");
            let new = bisync_workdir("docs");
            std::fs::create_dir_all(&old).unwrap();
            std::fs::create_dir_all(&new).unwrap();
            std::fs::write(old.join("x.lst"), "stale").unwrap();
            std::fs::write(new.join("x.lst"), "current").unwrap();

            migrate_bisync_workdir("docs").unwrap();

            assert_eq!(
                std::fs::read_to_string(new.join("x.lst")).unwrap(),
                "current"
            );
            assert!(old.exists());
        });
    }

    #[test]
    fn legacy_filter_hash_is_migrated_into_state() {
        with_xdg(|| {
            std::fs::create_dir_all(cache_dir()).unwrap();
            std::fs::write(cache_dir().join("filters.txt.md5"), "abc").unwrap();

            migrate_filters_hash().unwrap();

            let moved = filters_dir().join("filters.txt.md5");
            assert_eq!(std::fs::read_to_string(moved).unwrap(), "abc");
            assert!(!cache_dir().join("filters.txt.md5").exists());
        });
    }

    #[test]
    fn migration_without_a_legacy_workdir_does_nothing() {
        with_xdg(|| {
            migrate_bisync_workdir("docs").unwrap();
            assert!(!bisync_workdir("docs").exists());
        });
    }
}
