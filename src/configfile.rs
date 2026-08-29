//! Format-preserving edits to `config.toml`.
//!
//! The config file is expected to live in a dotfiles repo, hand-edited and version
//! controlled. A CLI that rewrote it by serialising a struct would strip comments and
//! reorder keys, producing a spurious diff every time. `toml_edit` mutates the parsed
//! document in place, so everything the user wrote survives.

use crate::error::{Error, Result};
use crate::paths;
use std::path::{Path, PathBuf};
use toml_edit::{value, DocumentMut, Item, Table};

const HEADER: &str = "\
# lodestone folders. See `lode add --help`.
#
# Prefer ~-relative paths: this file is meant to be shared across machines.
# Machine-specific overrides belong in config.local.toml (gitignored).
";

/// Where `add`/`forget` write: the explicit `--config` path, else the shared config.
pub fn target_path(explicit: Option<&Path>) -> PathBuf {
    explicit
        .map(PathBuf::from)
        .unwrap_or_else(|| paths::config_dir().join("config.toml"))
}

fn read_doc(path: &Path) -> Result<DocumentMut> {
    let raw = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => HEADER.to_string(),
        Err(e) => return Err(Error::io(path.display(), e)),
    };
    raw.parse::<DocumentMut>()
        .map_err(|e| Error::Config(format!("{}: {e}", path.display())))
}

fn write_doc(path: &Path, doc: &DocumentMut) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| Error::io(parent.display(), e))?;
    }
    // Write-then-rename: a half-written config would be worse than no edit at all.
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, doc.to_string()).map_err(|e| Error::io(tmp.display(), e))?;
    std::fs::rename(&tmp, path).map_err(|e| Error::io(path.display(), e))
}

/// Rewrite a path under `$HOME` as `~/...`.
///
/// Keeps the shared config portable: `/Users/me/silvermine` on a Mac and
/// `/home/me/silvermine` on Linux are the same `~/silvermine` stanza.
pub fn portable_path(p: &Path) -> String {
    let home = paths::home();
    match p.strip_prefix(&home) {
        Ok(rest) if !rest.as_os_str().is_empty() => format!("~/{}", rest.display()),
        _ => p.display().to_string(),
    }
}

/// Resolve a user-supplied path to something absolute and portable.
pub fn normalise_input(raw: &str) -> Result<String> {
    // A `~`-relative path is already portable; keep it verbatim.
    if raw == "~" || raw.starts_with("~/") {
        return Ok(raw.to_string());
    }
    let p = PathBuf::from(raw);
    let abs = if p.is_absolute() {
        p
    } else {
        std::env::current_dir()
            .map_err(|e| Error::io("cwd", e))?
            .join(p)
    };
    Ok(portable_path(&abs))
}

pub struct NewFolder<'a> {
    pub name: &'a str,
    pub local: &'a str,
    pub remote: &'a str,
    pub max_deletes: Option<usize>,
}

/// Insert a `[folder.<name>]` stanza. Fails if the name is already present.
pub fn add_folder(path: &Path, f: &NewFolder<'_>) -> Result<()> {
    if f.name.contains('/') || f.name.contains(std::path::MAIN_SEPARATOR) {
        return Err(Error::Config(format!(
            "folder name {:?} must not contain a path separator (it is used as a state directory name)",
            f.name
        )));
    }

    let mut doc = read_doc(path)?;
    let folders = doc
        .entry("folder")
        .or_insert(Item::Table(Table::new()))
        .as_table_mut()
        .ok_or_else(|| Error::Config(format!("{}: `folder` is not a table", path.display())))?;
    // Implicit so the file renders `[folder.silvermine]` rather than a bare `[folder]`.
    folders.set_implicit(true);

    if folders.contains_key(f.name) {
        return Err(Error::Config(format!(
            "folder {:?} is already configured in {}",
            f.name,
            path.display()
        )));
    }

    let mut t = Table::new();
    t["local"] = value(f.local);
    t["remote"] = value(f.remote);
    if let Some(n) = f.max_deletes {
        t["max_deletes"] = value(n as i64);
    }
    folders.insert(f.name, Item::Table(t));

    write_doc(path, &doc)
}

/// Remove a `[folder.<name>]` stanza. Returns false if it was not there.
pub fn remove_folder(path: &Path, name: &str) -> Result<bool> {
    let mut doc = read_doc(path)?;
    let Some(folders) = doc.get_mut("folder").and_then(|f| f.as_table_mut()) else {
        return Ok(false);
    };
    if folders.remove(name).is_none() {
        return Ok(false);
    }
    write_doc(path, &doc)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpfile(body: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        if !body.is_empty() {
            std::fs::write(&p, body).unwrap();
        }
        (dir, p)
    }

    fn add(p: &Path, name: &str, local: &str, remote: &str) -> Result<()> {
        add_folder(
            p,
            &NewFolder {
                name,
                local,
                remote,
                max_deletes: None,
            },
        )
    }

    #[test]
    fn creates_the_file_when_absent() {
        let (_d, p) = tmpfile("");
        add(&p, "silvermine", "~/silvermine", "per-gdrive:Silvermine").unwrap();
        let out = std::fs::read_to_string(&p).unwrap();
        assert!(out.contains("[folder.silvermine]"), "{out}");
        assert!(out.contains(r#"local = "~/silvermine""#), "{out}");
        assert!(out.contains("# lodestone folders"), "header missing: {out}");
        // Round-trips through the real loader.
        let parsed: crate::config::RawConfig = toml::from_str(&out).unwrap();
        assert!(parsed.folder.contains_key("silvermine"));
    }

    #[test]
    fn preserves_comments_and_existing_content() {
        let (_d, p) = tmpfile(
            r#"# my notes, please keep
[defaults]
max_deletes = 7   # deliberately low

[folder.existing]
local  = "~/existing"   # aligned on purpose
remote = "r:existing"
"#,
        );
        add(&p, "new", "~/new", "r:new").unwrap();
        let out = std::fs::read_to_string(&p).unwrap();

        assert!(out.contains("# my notes, please keep"), "{out}");
        assert!(
            out.contains("max_deletes = 7   # deliberately low"),
            "{out}"
        );
        assert!(
            out.contains(r#"local  = "~/existing"   # aligned on purpose"#),
            "{out}"
        );
        assert!(out.contains("[folder.new]"), "{out}");
    }

    #[test]
    fn refuses_a_duplicate_name() {
        let (_d, p) = tmpfile("");
        add(&p, "a", "~/a", "r:a").unwrap();
        let err = add(&p, "a", "~/other", "r:other").unwrap_err();
        assert!(err.to_string().contains("already configured"), "{err}");
        // The failed attempt must not have altered the existing entry.
        assert!(std::fs::read_to_string(&p).unwrap().contains("~/a"));
    }

    #[test]
    fn refuses_a_name_with_a_path_separator() {
        let (_d, p) = tmpfile("");
        let err = add(&p, "a/b", "~/a", "r:a").unwrap_err();
        assert!(err.to_string().contains("path separator"), "{err}");
    }

    #[test]
    fn max_deletes_is_written_only_when_given() {
        let (_d, p) = tmpfile("");
        add_folder(
            &p,
            &NewFolder {
                name: "a",
                local: "~/a",
                remote: "r:a",
                max_deletes: Some(25),
            },
        )
        .unwrap();
        assert!(std::fs::read_to_string(&p)
            .unwrap()
            .contains("max_deletes = 25"));

        add(&p, "b", "~/b", "r:b").unwrap();
        let out = std::fs::read_to_string(&p).unwrap();
        let b_stanza = out.split("[folder.b]").nth(1).unwrap();
        assert!(!b_stanza.contains("max_deletes"), "{out}");
    }

    #[test]
    fn removes_a_stanza_and_leaves_the_rest() {
        let (_d, p) = tmpfile("");
        add(&p, "a", "~/a", "r:a").unwrap();
        add(&p, "b", "~/b", "r:b").unwrap();

        assert!(remove_folder(&p, "a").unwrap());
        let out = std::fs::read_to_string(&p).unwrap();
        assert!(!out.contains("[folder.a]"), "{out}");
        assert!(out.contains("[folder.b]"), "{out}");

        // Removing something absent is not an error, just false.
        assert!(!remove_folder(&p, "nope").unwrap());
    }

    #[test]
    fn paths_under_home_become_tilde_relative() {
        let _g = crate::testlock::env_lock();
        std::env::set_var("HOME", "/home/alice");
        assert_eq!(
            portable_path(Path::new("/home/alice/silvermine")),
            "~/silvermine"
        );
        assert_eq!(portable_path(Path::new("/mnt/data/docs")), "/mnt/data/docs");
        // The home directory itself has no meaningful `~/` suffix; left as-is.
        assert_eq!(portable_path(Path::new("/home/alice")), "/home/alice");
    }

    #[test]
    fn input_paths_are_absolutised_and_made_portable() {
        let _g = crate::testlock::env_lock();
        std::env::set_var("HOME", "/home/alice");
        assert_eq!(normalise_input("~/silvermine").unwrap(), "~/silvermine");
        assert_eq!(normalise_input("/home/alice/docs").unwrap(), "~/docs");
        assert_eq!(normalise_input("/mnt/data").unwrap(), "/mnt/data");
    }
}
