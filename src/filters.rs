//! OS-junk filtering.
//!
//! This is deliberately **not** a filter engine. The rule set is compiled into the binary
//! and cannot be configured, which is both simpler and *safer* than a configurable one:
//!
//! bisync fingerprints its filter set and demands a `--resync` whenever it changes. A
//! configurable set would therefore drift between machines — filter `.DS_Store` on the Mac
//! but not on the Linux box and every machine switch forces a resync. Compiled in means
//! byte-identical by construction for a given lodestone version.
//!
//! The consequence is that **changing this list is a breaking change** requiring one
//! resync per folder per machine. The fingerprint is recorded in each snapshot so that
//! when rclone demands a resync, lodestone can say *why*.
//!
//! Some filtering is mandatory on a mixed fleet: macOS writes `.DS_Store` into every
//! directory opened in Finder, and AppleDouble `._*` files appear whenever a file with
//! extended attributes touches a non-native filesystem. Without this, that litter syncs to
//! the remote and lands on every Linux machine.

use crate::error::{Error, Result};
use std::path::{Path, PathBuf};

/// Exclusion patterns in rclone filter syntax.
///
/// A pattern without a `/` matches at any depth, which is what we want — `.DS_Store`
/// appears in every directory, not just the root.
pub const RULES: &[&str] = &[
    // macOS
    "- .DS_Store",
    "- ._*",
    "- .Spotlight-V100/**",
    "- .fseventsd/**",
    "- .Trashes/**",
    "- .TemporaryItems/**",
    "- .apdisk",
    // Linux desktops
    "- .directory",
    "- .Trash-*/**",
    // Windows, in case a file ever lands on the remote from one
    "- Thumbs.db",
    "- desktop.ini",
    "- ~$*",
];

/// The filter file contents handed to rclone.
pub fn body() -> String {
    let mut s = String::new();
    for rule in RULES {
        s.push_str(rule);
        s.push('\n');
    }
    s
}

/// Stable identifier for the active rule set.
///
/// FNV-1a over the rendered body. This only needs to detect *change*, not resist attack,
/// so a cryptographic hash would be needless weight.
pub fn fingerprint() -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in body().as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x1000_0000_01b3);
    }
    format!("fnv1a64:{hash:016x}")
}

/// Materialise the filter file, returning its path.
///
/// Written once per run into the cache directory. Content is what matters to rclone —
/// bisync hashes the rules, not the path.
pub fn write_file(dir: &Path) -> Result<PathBuf> {
    std::fs::create_dir_all(dir).map_err(|e| Error::io(dir.display(), e))?;
    let path = dir.join("filters.txt");
    let want = body();
    // Avoid rewriting an identical file so the mtime stays stable across runs.
    if std::fs::read_to_string(&path).ok().as_deref() != Some(want.as_str()) {
        std::fs::write(&path, &want).map_err(|e| Error::io(path.display(), e))?;
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_renders_one_rule_per_line() {
        let b = body();
        assert_eq!(b.lines().count(), RULES.len());
        assert!(b.starts_with("- .DS_Store\n"));
        assert!(b.ends_with('\n'), "rclone wants a trailing newline");
    }

    #[test]
    fn every_rule_is_an_exclusion() {
        // An accidental include rule would silently invert the meaning of the whole file.
        for rule in RULES {
            assert!(rule.starts_with("- "), "not an exclusion: {rule}");
        }
    }

    #[test]
    fn fingerprint_is_stable_and_sensitive() {
        // Stability matters: an unstable fingerprint would demand a resync on every run.
        assert_eq!(fingerprint(), fingerprint());
        assert!(fingerprint().starts_with("fnv1a64:"));

        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for byte in "- .DS_Store\n".as_bytes() {
            h ^= *byte as u64;
            h = h.wrapping_mul(0x1000_0000_01b3);
        }
        assert_ne!(
            fingerprint(),
            format!("fnv1a64:{h:016x}"),
            "a different rule set must produce a different fingerprint"
        );
    }

    #[test]
    fn writing_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let a = write_file(tmp.path()).unwrap();
        let before = std::fs::metadata(&a).unwrap().modified().unwrap();
        let b = write_file(tmp.path()).unwrap();
        assert_eq!(a, b);
        assert_eq!(std::fs::metadata(&b).unwrap().modified().unwrap(), before);
        assert_eq!(std::fs::read_to_string(&b).unwrap(), body());
    }
}
