//! Configuration.
//!
//! Two layers, both TOML:
//!
//! * `~/.config/lode/config.toml` — shared, expected to live in a dotfiles repo.
//! * `~/.config/lode/config.local.toml` — machine-local overrides, gitignored. Exists so
//!   one machine can put a folder somewhere other than `~/<name>` without forking the
//!   shared file.
//!
//! Precedence: CLI flags > `LODE_*` env > local > shared > built-in defaults.

use crate::error::{Error, Result};
use crate::paths;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub const DEFAULT_MAX_DELETES: usize = 10;

/// The hash algorithm used for rename matching. md5 is chosen because Google Drive
/// serves md5 for binary files; `lsjson` without `--hash-type` computes *every*
/// algorithm, which is needlessly expensive.
pub const HASH_TYPE: &str = "md5";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RawConfig {
    #[serde(default)]
    pub defaults: Defaults,
    /// `[folder.<name>]` stanzas.
    #[serde(default)]
    pub folder: BTreeMap<String, RawFolder>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Defaults {
    pub max_deletes: Option<usize>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RawFolder {
    pub local: Option<String>,
    pub remote: Option<String>,
    pub max_deletes: Option<usize>,
}

/// A fully resolved folder: every field present, `~` expanded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Folder {
    pub name: String,
    pub local: PathBuf,
    pub remote: String,
    pub max_deletes: usize,
}

impl Folder {
    /// The remote name portion of `remote:path`, used to validate against
    /// `rclone listremotes`. Returns `None` for a bare local path (used in tests).
    pub fn remote_name(&self) -> Option<&str> {
        let (name, _) = self.remote.split_once(':')?;
        if name.is_empty() {
            None
        } else {
            Some(name)
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Config {
    pub folders: Vec<Folder>,
}

impl Config {
    pub fn get(&self, name: &str) -> Result<&Folder> {
        self.folders
            .iter()
            .find(|f| f.name == name)
            .ok_or_else(|| Error::UnknownFolder(name.to_string()))
    }

    /// Resolve the folder whose `local` path contains `cwd`, for `lode status .`.
    pub fn containing(&self, cwd: &Path) -> Result<&Folder> {
        self.folders
            .iter()
            .find(|f| paths::is_inside(cwd, &f.local))
            .ok_or_else(|| {
                Error::Config(format!(
                    "{} is not inside any configured folder",
                    cwd.display()
                ))
            })
    }

    /// Load from the standard locations, or from an explicit path if given.
    pub fn load(explicit: Option<&Path>) -> Result<Self> {
        let (shared, local) = match explicit {
            Some(p) => (read_toml(p)?.unwrap_or_default(), RawConfig::default()),
            None => {
                let dir = paths::config_dir();
                (
                    read_toml(&dir.join("config.toml"))?.unwrap_or_default(),
                    read_toml(&dir.join("config.local.toml"))?.unwrap_or_default(),
                )
            }
        };
        Self::resolve(shared, local)
    }

    /// Merge the two layers and validate. Public for testing.
    pub fn resolve(shared: RawConfig, local: RawConfig) -> Result<Self> {
        let max_deletes_default = local
            .defaults
            .max_deletes
            .or(shared.defaults.max_deletes)
            .unwrap_or(DEFAULT_MAX_DELETES);

        // A folder may be declared in either layer; the local layer overrides field by
        // field rather than wholesale, so overriding `local` does not require repeating
        // `remote`.
        let mut names: Vec<String> = shared.folder.keys().cloned().collect();
        for k in local.folder.keys() {
            if !names.contains(k) {
                names.push(k.clone());
            }
        }

        let mut folders = Vec::new();
        for name in names {
            let s = shared.folder.get(&name);
            let l = local.folder.get(&name);
            let pick = |f: fn(&RawFolder) -> Option<String>| -> Option<String> {
                l.and_then(f).or_else(|| s.and_then(f))
            };

            let local_path = pick(|r| r.local.clone()).ok_or_else(|| {
                Error::Config(format!("folder {name:?} is missing required key `local`"))
            })?;
            let remote = pick(|r| r.remote.clone()).ok_or_else(|| {
                Error::Config(format!("folder {name:?} is missing required key `remote`"))
            })?;
            let max_deletes = l
                .and_then(|r| r.max_deletes)
                .or_else(|| s.and_then(|r| r.max_deletes))
                .unwrap_or(max_deletes_default);

            if name.contains('/') || name.contains(std::path::MAIN_SEPARATOR) {
                return Err(Error::Config(format!(
                    "folder name {name:?} must not contain a path separator (it is used as a state directory name)"
                )));
            }

            folders.push(Folder {
                name,
                local: paths::expand_tilde(&local_path),
                remote,
                max_deletes,
            });
        }

        let cfg = Config { folders };
        cfg.validate_state_not_synced()?;
        Ok(cfg)
    }

    /// Refuse to run if lodestone's own state would live inside a synced folder.
    fn validate_state_not_synced(&self) -> Result<()> {
        let state = paths::state_dir();
        for f in &self.folders {
            if paths::is_inside(&state, &f.local) {
                return Err(Error::StateInsideSyncedFolder {
                    state: state.display().to_string(),
                    folder: f.local.display().to_string(),
                });
            }
        }
        Ok(())
    }
}

fn read_toml(path: &Path) -> Result<Option<RawConfig>> {
    match std::fs::read_to_string(path) {
        Ok(s) => toml::from_str(&s)
            .map(Some)
            .map_err(|e| Error::Config(format!("{}: {e}", path.display()))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(Error::io(path.display(), e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> RawConfig {
        toml::from_str(s).unwrap()
    }

    #[test]
    fn resolves_a_simple_folder() {
        let _g = crate::testlock::env_lock();
        std::env::set_var("HOME", "/home/alice");
        let cfg = Config::resolve(
            parse(
                r#"
                [folder.silvermine]
                local = "~/silvermine"
                remote = "per-gdrive:Silvermine"
                "#,
            ),
            RawConfig::default(),
        )
        .unwrap();
        let f = cfg.get("silvermine").unwrap();
        assert_eq!(f.local, PathBuf::from("/home/alice/silvermine"));
        assert_eq!(f.remote, "per-gdrive:Silvermine");
        assert_eq!(f.max_deletes, DEFAULT_MAX_DELETES);
        assert_eq!(f.remote_name(), Some("per-gdrive"));
    }

    #[test]
    fn local_layer_overrides_field_by_field() {
        let _g = crate::testlock::env_lock();
        std::env::set_var("HOME", "/home/alice");
        let cfg = Config::resolve(
            parse(
                r#"
                [folder.silvermine]
                local = "~/silvermine"
                remote = "per-gdrive:Silvermine"
                "#,
            ),
            // Overriding only `local` must not require restating `remote`.
            parse(
                r#"
                [folder.silvermine]
                local = "/mnt/data/silvermine"
                "#,
            ),
        )
        .unwrap();
        let f = cfg.get("silvermine").unwrap();
        assert_eq!(f.local, PathBuf::from("/mnt/data/silvermine"));
        assert_eq!(f.remote, "per-gdrive:Silvermine");
    }

    #[test]
    fn max_deletes_precedence() {
        let _g = crate::testlock::env_lock();
        std::env::set_var("HOME", "/home/alice");
        let cfg = Config::resolve(
            parse(
                r#"
                [defaults]
                max_deletes = 5
                [folder.a]
                local = "~/a"
                remote = "r:a"
                [folder.b]
                local = "~/b"
                remote = "r:b"
                max_deletes = 99
                "#,
            ),
            RawConfig::default(),
        )
        .unwrap();
        assert_eq!(cfg.get("a").unwrap().max_deletes, 5);
        assert_eq!(cfg.get("b").unwrap().max_deletes, 99);
    }

    #[test]
    fn missing_required_keys_are_rejected() {
        let _g = crate::testlock::env_lock();
        std::env::set_var("HOME", "/home/alice");
        let err = Config::resolve(
            parse(
                r#"[folder.a]
            local = "~/a""#,
            ),
            RawConfig::default(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("remote"), "{err}");
    }

    #[test]
    fn folder_names_with_separators_are_rejected() {
        let _g = crate::testlock::env_lock();
        std::env::set_var("HOME", "/home/alice");
        let err = Config::resolve(
            parse(
                r#"
                [folder."a/b"]
                local = "~/a"
                remote = "r:a"
                "#,
            ),
            RawConfig::default(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("path separator"), "{err}");
    }

    #[test]
    fn state_inside_a_synced_folder_is_refused() {
        let _g = crate::testlock::env_lock();
        std::env::set_var("HOME", "/home/alice");
        std::env::set_var("XDG_STATE_HOME", "/home/alice/silvermine/state");
        let err = Config::resolve(
            parse(
                r#"
                [folder.silvermine]
                local = "~/silvermine"
                remote = "r:s"
                "#,
            ),
            RawConfig::default(),
        )
        .unwrap_err();
        std::env::remove_var("XDG_STATE_HOME");
        assert!(
            matches!(err, Error::StateInsideSyncedFolder { .. }),
            "{err}"
        );
    }
}
