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

/// `$XDG_CACHE_HOME/lode` (default `~/.cache/lode`). Holds rclone bisync workdirs.
pub fn cache_dir() -> PathBuf {
    xdg("XDG_CACHE_HOME", ".cache").join("lode")
}

pub fn folder_state_dir(folder: &str) -> PathBuf {
    state_dir().join("folders").join(folder)
}

pub fn snapshot_path(folder: &str) -> PathBuf {
    folder_state_dir(folder).join("snapshot.json")
}

pub fn bisync_workdir(folder: &str) -> PathBuf {
    cache_dir().join("bisync").join(folder)
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
}
