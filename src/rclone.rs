//! The rclone adapter.
//!
//! lodestone shells out to the `rclone` binary and never links it. rclone owns
//! `rclone.conf`, every remote, and all OAuth; lodestone only references remotes by name.
//!
//! ## Why `--force` is always passed to bisync
//!
//! bisync applies its own *percentage-based* safety check during delta detection, before
//! the sync stage where `--track-renames` operates. A moved subtree therefore reads as a
//! mass deletion and aborts the run — verified empirically:
//!
//! ```text
//! ERROR : Safety abort: too many deletes (>50%, 12 of 12) on Path1 ...
//! ```
//!
//! With `--force`, the same run produces 12 server-side moves and zero transfers. So
//! rclone's blunt guard is unusable for a rename-heavy workload, and lodestone
//! deliberately disables it — substituting its own precise guard, computed in the plan
//! phase against true deletes only. See `docs/TDD.md` §Safety.

use crate::config::HASH_TYPE;
use crate::error::{Error, Result};
use crate::snapshot::{Entry, Listing};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::{Command, Stdio};

/// Minimum supported rclone. Below this, the `--conflict-resolve` family and the modern
/// bisync deltas are absent, so lodestone's conflict semantics would silently not exist.
pub const MIN_VERSION: Version = Version {
    major: 1,
    minor: 66,
    patch: 0,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "v{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// Parse the first line of `rclone version`, e.g. `rclone v1.73.3`.
///
/// Tolerates suffixes such as `-beta.1234.abcdef` and `-DEV`.
pub fn parse_version(output: &str) -> Result<Version> {
    let first = output.lines().next().unwrap_or_default();
    let token = first
        .split_whitespace()
        .find(|t| {
            t.starts_with('v') && t.len() > 1 && t[1..].starts_with(|c: char| c.is_ascii_digit())
        })
        .ok_or_else(|| Error::RcloneVersionUnparseable(first.to_string()))?;

    let core = &token[1..];
    // Cut any pre-release/build suffix.
    let core = core.split(['-', '+']).next().unwrap_or(core);
    let mut it = core.split('.');
    let mut next = |what: &str| -> Result<u32> {
        it.next()
            .and_then(|s| s.parse::<u32>().ok())
            .ok_or_else(|| Error::RcloneVersionUnparseable(format!("{first} ({what})")))
    };
    Ok(Version {
        major: next("major")?,
        minor: next("minor")?,
        // A two-component version like `v1.66` is valid; treat the patch as 0.
        patch: it.next().and_then(|s| s.parse::<u32>().ok()).unwrap_or(0),
    })
}

/// POSIX per-component filename limit. Both APFS and ext4 use 255 bytes.
pub const FILENAME_LIMIT: usize = 255;

/// Length of the workdir filename bisync will derive from a path pair.
///
/// bisync names its listing and lock files by flattening *both* full paths into a single
/// filename (`path1_flattened..path2_flattened.lck`). With deep paths this breaches the
/// 255-byte limit and the run dies with `file name too long` before doing any work.
/// Discovered the hard way while running the rename harness under a deep scratch dir.
pub fn session_name_len(path1: &str, path2: &str) -> usize {
    // `..` separator plus the longest suffix rclone appends (`.lck`).
    flatten(path1).len() + 2 + flatten(path2).len() + 4
}

fn flatten(p: &str) -> String {
    p.trim_start_matches('/')
        .chars()
        .map(|c| if c == '/' || c == ':' { '_' } else { c })
        .collect()
}

#[derive(Debug, Clone)]
pub struct Rclone {
    pub binary: PathBuf,
    /// Path to the compiled-in OS-junk filter file. Applied to *both* listing and sync:
    /// if the plan phase saw files that bisync then filtered out, the two would disagree
    /// about what changed.
    pub filter_file: Option<String>,
}

/// Raw `lsjson` record. Only the fields lodestone needs are declared.
#[derive(Debug, Deserialize)]
struct LsJsonItem {
    #[serde(rename = "Path")]
    path: String,
    #[serde(rename = "Size")]
    size: i64,
    #[serde(rename = "ModTime")]
    #[serde(default)]
    mod_time: String,
    #[serde(rename = "IsDir")]
    #[serde(default)]
    is_dir: bool,
    #[serde(rename = "Hashes")]
    #[serde(default)]
    hashes: BTreeMap<String, String>,
}

impl Rclone {
    /// Locate rclone: `$LODE_RCLONE` if set, else `rclone` on `PATH`.
    pub fn discover() -> Result<Self> {
        if let Some(p) = std::env::var_os("LODE_RCLONE") {
            let p = PathBuf::from(p);
            if !p.exists() {
                return Err(Error::RcloneMissing);
            }
            return Ok(Self {
                binary: p,
                filter_file: None,
            });
        }
        let ok = Command::new("rclone")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if ok {
            Ok(Self {
                binary: PathBuf::from("rclone"),
                filter_file: None,
            })
        } else {
            Err(Error::RcloneMissing)
        }
    }

    /// Attach the filter file used for every subsequent listing and sync.
    pub fn with_filters(mut self, path: impl Into<String>) -> Self {
        self.filter_file = Some(path.into());
        self
    }

    fn run(&self, args: &[&str]) -> Result<String> {
        let out = Command::new(&self.binary)
            .args(args)
            .output()
            .map_err(|e| Error::io(self.binary.display(), e))?;
        if !out.status.success() {
            return Err(Error::RcloneFailed {
                code: out.status.code().unwrap_or(-1),
                stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
            });
        }
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    }

    pub fn version(&self) -> Result<Version> {
        parse_version(&self.run(&["version"])?)
    }

    /// Hard-refuse below the floor. A silently-degraded conflict policy is exactly the
    /// failure this design exists to prevent, so this is an error, not a warning.
    pub fn require_min_version(&self) -> Result<Version> {
        let found = self.version()?;
        if found < MIN_VERSION {
            return Err(Error::RcloneTooOld {
                found: found.to_string(),
                required: MIN_VERSION.to_string(),
            });
        }
        Ok(found)
    }

    /// Remote names from `rclone listremotes`, without the trailing colon.
    pub fn list_remotes(&self) -> Result<Vec<String>> {
        Ok(self
            .run(&["listremotes"])?
            .lines()
            .map(|l| l.trim().trim_end_matches(':').to_string())
            .filter(|l| !l.is_empty())
            .collect())
    }

    /// Recursive file listing with content hashes.
    ///
    /// `--hash-type md5` is not optional: without it `lsjson` computes *every* supported
    /// algorithm (blake3, sha512, whirlpool, ...), which on a local path means reading
    /// every file many times over.
    pub fn lsjson(&self, path: &str) -> Result<Listing> {
        let mut args = vec![
            "lsjson",
            "--recursive",
            "--files-only",
            "--hash",
            "--hash-type",
            HASH_TYPE,
        ];
        // `lsjson` has no --filters-file; --filter-from is the equivalent.
        if let Some(f) = &self.filter_file {
            args.push("--filter-from");
            args.push(f);
        }
        args.push(path);
        let raw = self.run(&args)?;
        let items: Vec<LsJsonItem> = serde_json::from_str(&raw)?;
        Ok(items
            .into_iter()
            .filter(|i| !i.is_dir)
            .map(|i| {
                let hash = i.hashes.get(HASH_TYPE).cloned();
                (i.path, Entry::new(i.size.max(0) as u64, i.mod_time, hash))
            })
            .collect())
    }

    /// Flags shared by every bisync invocation. See the module docs for `--force`.
    pub fn bisync_base_args<'a>(&'a self, workdir: &'a str) -> Vec<&'a str> {
        let mut args = vec![
            "bisync",
            "--workdir",
            workdir,
            // Disable rclone's percentage guard; lodestone applies its own, on true
            // deletes only, before ever reaching this point.
            "--force",
            // Collapse moves into server-side renames instead of re-uploading.
            "--track-renames",
            "--track-renames-strategy",
            "hash",
            // Never let rclone silently pick a winner. lodestone aborts on conflicts in
            // the plan phase; this is the backstop if one appears between plan and apply.
            "--conflict-resolve",
            "none",
            "--stats",
            "0",
        ];
        // bisync has its own --filters-file, which is what it fingerprints to decide
        // whether a resync is required.
        if let Some(f) = &self.filter_file {
            args.push("--filters-file");
            args.push(f);
        }
        args
    }

    /// Run bisync with the given path pair and extra flags, returning combined output.
    pub fn bisync(
        &self,
        path1: &str,
        path2: &str,
        workdir: &str,
        extra: &[&str],
    ) -> Result<String> {
        let mut args = self.bisync_base_args(workdir);
        args.push(path1);
        args.push(path2);
        args.extend_from_slice(extra);
        let out = Command::new(&self.binary)
            .args(&args)
            .output()
            .map_err(|e| Error::io(self.binary.display(), e))?;
        let combined = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        if !out.status.success() {
            return Err(Error::RcloneFailed {
                code: out.status.code().unwrap_or(-1),
                stderr: combined.trim().to_string(),
            });
        }
        Ok(combined)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_real_version_banner() {
        let banner = "rclone v1.73.3\n- os/version: darwin 26.5.2 (64 bit)\n- os/arch: arm64";
        assert_eq!(
            parse_version(banner).unwrap(),
            Version {
                major: 1,
                minor: 73,
                patch: 3
            }
        );
    }

    #[test]
    fn parses_prerelease_and_dev_versions() {
        assert_eq!(
            parse_version("rclone v1.66.0-beta.7000.abcdef").unwrap(),
            Version {
                major: 1,
                minor: 66,
                patch: 0
            }
        );
        assert_eq!(
            parse_version("rclone v1.67.0-DEV").unwrap(),
            Version {
                major: 1,
                minor: 67,
                patch: 0
            }
        );
        // Two-component versions are tolerated.
        assert_eq!(
            parse_version("rclone v1.66").unwrap(),
            Version {
                major: 1,
                minor: 66,
                patch: 0
            }
        );
    }

    #[test]
    fn rejects_unparseable_banners() {
        assert!(parse_version("").is_err());
        assert!(parse_version("something else entirely").is_err());
        assert!(parse_version("rclone version unknown").is_err());
    }

    #[test]
    fn version_ordering_drives_the_floor_check() {
        assert!(parse_version("rclone v1.65.2").unwrap() < MIN_VERSION);
        assert!(parse_version("rclone v1.66.0").unwrap() >= MIN_VERSION);
        assert!(parse_version("rclone v1.73.3").unwrap() >= MIN_VERSION);
        assert!(parse_version("rclone v2.0.0").unwrap() >= MIN_VERSION);
    }

    #[test]
    fn session_name_length_accounts_for_both_sides() {
        // Regression guard for a failure found while running the rename harness under a
        // deep scratch directory: rclone died with "file name too long" because it
        // encodes both full paths into one workdir filename.
        let short = session_name_len("/home/alice/silvermine", "per-gdrive:Silvermine");
        assert!(short < FILENAME_LIMIT, "{short}");

        let deep_a = format!("/home/alice/{}", "nested/".repeat(20));
        let deep_b = format!("remote:{}", "nested/".repeat(20));
        assert!(session_name_len(&deep_a, &deep_b) > FILENAME_LIMIT);

        // Separators are flattened rather than dropped, so length is preserved.
        assert_eq!(flatten("/a/b:c"), "a_b_c");
    }

    #[test]
    fn base_args_disable_rclone_guard_and_enable_rename_tracking() {
        let r = Rclone {
            binary: "rclone".into(),
            filter_file: None,
        };
        let args = r.bisync_base_args("/tmp/wd");
        assert!(args.contains(&"--force"));
        assert!(args.contains(&"--track-renames"));
        assert!(args.contains(&"--conflict-resolve"));
        assert!(args.contains(&"none"));
        assert!(
            !args.contains(&"--resilient"),
            "recovery must be manual and loud"
        );
        // Without a filter file configured, no filter flag may appear — an empty
        // --filters-file would change bisync's fingerprint and force a resync.
        assert!(!args.contains(&"--filters-file"));
    }

    #[test]
    fn filters_are_passed_to_bisync_when_configured() {
        let r = Rclone {
            binary: "rclone".into(),
            filter_file: None,
        }
        .with_filters("/cache/filters.txt");
        let args = r.bisync_base_args("/tmp/wd");
        let i = args
            .iter()
            .position(|a| *a == "--filters-file")
            .expect("filter flag missing");
        assert_eq!(args[i + 1], "/cache/filters.txt");
    }
}
