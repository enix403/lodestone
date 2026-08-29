//! End-to-end tests: the real `lode` binary, the real `rclone`, no network.
//!
//! Two local directories are a perfectly valid bisync pair, so the whole init → status
//! flow can be exercised offline. Each test gets its own temp tree and passes `XDG_*` via
//! the child's environment, so they are safe to run in parallel.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn rclone_available() -> bool {
    Command::new("rclone")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// A scratch world: config file, local dir, "remote" dir, isolated XDG state.
struct World {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    config: PathBuf,
    local: PathBuf,
}

impl World {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        // macOS hands out /var/... which is a symlink to /private/var. rclone reports
        // resolved paths, so canonicalise up front to keep comparisons honest.
        let root = tmp.path().canonicalize().unwrap();
        let local = root.join("local");
        let remote = root.join("remote");
        std::fs::create_dir_all(&local).unwrap();
        std::fs::create_dir_all(&remote).unwrap();

        let config = root.join("config.toml");
        std::fs::write(
            &config,
            format!(
                "[folder.docs]\nlocal = \"{}\"\nremote = \"{}\"\nmax_deletes = 3\n",
                local.display(),
                remote.display()
            ),
        )
        .unwrap();

        World {
            _tmp: tmp,
            root,
            config,
            local,
        }
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_lode"))
            .args(args)
            .arg("--config")
            .arg(&self.config)
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env("XDG_CACHE_HOME", self.root.join("cache"))
            .env("XDG_CONFIG_HOME", self.root.join("xdgconfig"))
            .output()
            .expect("failed to run lode")
    }

    fn stdout(&self, args: &[&str]) -> String {
        let out = self.run(args);
        String::from_utf8_lossy(&out.stdout).to_string()
    }

    fn seed(&self, dir: &str, n: usize) {
        let d = self.local.join(dir);
        std::fs::create_dir_all(&d).unwrap();
        for i in 1..=n {
            // Distinct content per file, so hash matching has something to match on.
            let body: Vec<u8> = (0..1024u32)
                .map(|b| (b.wrapping_mul(i as u32) % 251) as u8)
                .collect();
            std::fs::write(d.join(format!("doc{i}.pdf")), body).unwrap();
        }
    }
}

fn assert_ok(out: &Output, what: &str) {
    assert!(
        out.status.success(),
        "{what} failed ({:?})\nstdout:\n{}\nstderr:\n{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

fn json(out: &Output) -> serde_json::Value {
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "not JSON ({e})\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    })
}

#[test]
fn init_establishes_a_baseline_and_status_is_clean() {
    if !rclone_available() {
        eprintln!("skipping: rclone not installed");
        return;
    }
    let w = World::new();
    w.seed("inbox", 5);

    let out = w.run(&["init"]);
    assert_ok(&out, "init");
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("5 file(s)"),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );

    // The remote side really did receive the files.
    assert_eq!(count_pdfs(&w.root.join("remote")), 5);

    let out = w.run(&["status"]);
    assert_ok(&out, "status");
    assert!(String::from_utf8_lossy(&out.stdout).contains("up to date"));

    // And the folder now reports as ready.
    assert!(w.stdout(&["folders"]).contains("ready"));
}

#[test]
fn a_moved_subtree_reads_as_renames_with_no_true_deletes() {
    if !rclone_available() {
        eprintln!("skipping: rclone not installed");
        return;
    }
    let w = World::new();
    w.seed("inbox", 12);
    assert_ok(&w.run(&["init"]), "init");

    // The reorganisation this whole design exists to handle.
    std::fs::create_dir_all(w.local.join("archive")).unwrap();
    std::fs::rename(w.local.join("inbox"), w.local.join("archive/2024")).unwrap();

    let out = w.run(&["status", "--json"]);
    let v = json(&out);
    let local = &v[0]["local"];

    assert_eq!(
        local["renamed"].as_array().unwrap().len(),
        12,
        "expected 12 renames: {v}"
    );
    assert_eq!(
        local["true_deletes"], 0,
        "a move must never count as a delete: {v}"
    );
    assert_eq!(local["added"].as_array().unwrap().len(), 0);
    assert_eq!(local["deleted"].as_array().unwrap().len(), 0);

    // max_deletes is 3, far below the 12 paths that moved — the guard must not fire.
    assert_eq!(v[0]["blocked"], serde_json::Value::Null);
    assert_ok(&out, "status after reorg");
}

#[test]
fn a_real_mass_deletion_trips_the_guard() {
    if !rclone_available() {
        eprintln!("skipping: rclone not installed");
        return;
    }
    let w = World::new();
    w.seed("inbox", 10);
    assert_ok(&w.run(&["init"]), "init");

    // Content vanishes entirely — the "folder didn't mount" catastrophe.
    std::fs::remove_dir_all(w.local.join("inbox")).unwrap();

    let out = w.run(&["status", "--json"]);
    let v = json(&out);
    assert_eq!(v[0]["local"]["true_deletes"], 10, "{v}");
    assert!(
        v[0]["blocked"]
            .as_str()
            .unwrap_or_default()
            .contains("delete guard"),
        "{v}"
    );
    // Exit code 11 is the documented delete-guard contract.
    assert_eq!(out.status.code(), Some(11));
}

#[test]
fn concurrent_edits_on_both_sides_are_reported_as_a_conflict() {
    if !rclone_available() {
        eprintln!("skipping: rclone not installed");
        return;
    }
    let w = World::new();
    w.seed("inbox", 3);
    assert_ok(&w.run(&["init"]), "init");

    // Same path, different content, changed on both sides since the baseline.
    std::fs::write(w.local.join("inbox/doc1.pdf"), b"local version").unwrap();
    std::fs::write(
        w.root.join("remote/inbox/doc1.pdf"),
        b"remote version, different",
    )
    .unwrap();

    let out = w.run(&["status", "--json"]);
    let v = json(&out);
    let conflicts = v[0]["conflicts"].as_array().unwrap();
    assert_eq!(conflicts.len(), 1, "{v}");
    assert_eq!(conflicts[0]["path"], "inbox/doc1.pdf");
    // Exit code 10 is the documented conflict contract.
    assert_eq!(out.status.code(), Some(10));
}

#[test]
fn status_reports_an_uninitialised_folder_clearly() {
    if !rclone_available() {
        eprintln!("skipping: rclone not installed");
        return;
    }
    let w = World::new();
    w.seed("inbox", 1);
    let out = w.run(&["status"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("lode init docs"), "{text}");
}

#[test]
fn a_foreign_snapshot_is_refused() {
    if !rclone_available() {
        eprintln!("skipping: rclone not installed");
        return;
    }
    let w = World::new();
    w.seed("inbox", 2);
    assert_ok(&w.run(&["init"]), "init");

    // Simulate a snapshot that travelled from another machine — which is exactly what
    // would happen if the state directory were ever synced.
    let snap = w.root.join("state/lode/folders/docs/snapshot.json");
    let body = std::fs::read_to_string(&snap).unwrap();
    let mut v: serde_json::Value = serde_json::from_str(&body).unwrap();
    v["machine_id"] = serde_json::Value::String("some-other-laptop-deadbeef".into());
    std::fs::write(&snap, serde_json::to_string_pretty(&v).unwrap()).unwrap();

    let out = w.run(&["status"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("machine"), "{text}");
    assert!(text.contains("never be synced"), "{text}");
}

fn count_pdfs(dir: &Path) -> usize {
    let mut n = 0;
    let Ok(rd) = std::fs::read_dir(dir) else {
        return 0;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            n += count_pdfs(&p);
        } else if p.extension().is_some_and(|x| x == "pdf") {
            n += 1;
        }
    }
    n
}
