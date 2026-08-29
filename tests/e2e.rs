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
        Self::with_folders(&["docs"])
    }

    /// One temp tree holding `local`/`remote` directory pairs for each named folder, plus
    /// a config file describing them.
    fn with_folders(names: &[&str]) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        // macOS hands out /var/... which is a symlink to /private/var. rclone reports
        // resolved paths, so canonicalise up front to keep comparisons honest.
        let root = tmp.path().canonicalize().unwrap();

        let mut config_body = String::new();
        for name in names {
            let local = root.join(name).join("local");
            let remote = root.join(name).join("remote");
            std::fs::create_dir_all(&local).unwrap();
            std::fs::create_dir_all(&remote).unwrap();
            config_body.push_str(&format!(
                "[folder.{name}]\nlocal = \"{}\"\nremote = \"{}\"\nmax_deletes = 3\n\n",
                local.display(),
                remote.display()
            ));
        }

        let config = root.join("config.toml");
        std::fs::write(&config, config_body).unwrap();
        let local = root.join(names[0]).join("local");

        World {
            _tmp: tmp,
            root,
            config,
            local,
        }
    }

    /// A world with no folders configured yet, for exercising `add`.
    fn empty() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let config = root.join("config.toml");
        std::fs::write(&config, "# empty\n").unwrap();
        let local = root.join("unused");
        World {
            _tmp: tmp,
            root,
            config,
            local,
        }
    }

    fn local_of(&self, folder: &str) -> PathBuf {
        self.root.join(folder).join("local")
    }

    fn remote_of(&self, folder: &str) -> PathBuf {
        self.root.join(folder).join("remote")
    }

    fn seed_in(&self, folder: &str, dir: &str, n: usize) {
        let d = self.local_of(folder).join(dir);
        std::fs::create_dir_all(&d).unwrap();
        for i in 1..=n {
            let body: Vec<u8> = (0..1024u32)
                .map(|b| (b.wrapping_mul(i as u32) % 251) as u8)
                .collect();
            std::fs::write(d.join(format!("doc{i}.pdf")), body).unwrap();
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
    assert_eq!(count_pdfs(&w.remote_of("docs")), 5);

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
        w.remote_of("docs").join("inbox/doc1.pdf"),
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

// ---------------------------------------------------------------------------
// Apply phase: sync / push / pull
// ---------------------------------------------------------------------------

#[test]
fn sync_applies_a_reorg_as_server_side_moves() {
    if !rclone_available() {
        eprintln!("skipping: rclone not installed");
        return;
    }
    let w = World::new();
    w.seed("inbox", 12);
    assert_ok(&w.run(&["init"]), "init");

    std::fs::create_dir_all(w.local.join("archive")).unwrap();
    std::fs::rename(w.local.join("inbox"), w.local.join("archive/2024")).unwrap();

    let out = w.run(&["sync", "--json"]);
    assert_ok(&out, "sync");
    let v = json(&out);

    // The whole point: moves cost metadata operations, not transfers.
    assert_eq!(v[0]["applied"], true, "{v}");
    assert_eq!(v[0]["moved_server_side"], 12, "{v}");
    assert_eq!(v[0]["transferred"], 0, "no bytes should be re-sent: {v}");

    // The remote really was reorganised, not re-uploaded alongside the old tree.
    assert_eq!(count_pdfs(&w.remote_of("docs").join("archive/2024")), 12);
    assert!(!w.remote_of("docs").join("inbox").exists());

    // And the snapshot advanced, so a second run has nothing to do.
    let out = w.run(&["status"]);
    assert_ok(&out, "status after sync");
    assert!(String::from_utf8_lossy(&out.stdout).contains("up to date"));
}

#[test]
fn sync_propagates_a_new_file_and_then_has_nothing_to_do() {
    if !rclone_available() {
        eprintln!("skipping: rclone not installed");
        return;
    }
    let w = World::new();
    w.seed("inbox", 2);
    assert_ok(&w.run(&["init"]), "init");

    std::fs::write(w.local.join("inbox/new.pdf"), b"a brand new document").unwrap();
    assert_ok(&w.run(&["push"]), "push");
    assert!(w.remote_of("docs").join("inbox/new.pdf").exists());

    // Idempotence: applying again must be a no-op, not a re-transfer.
    let out = w.run(&["sync", "--json"]);
    assert_ok(&out, "second sync");
    let v = json(&out);
    assert_eq!(v[0]["applied"], false, "{v}");
    assert_eq!(v[0]["reason"], "already up to date", "{v}");
}

#[test]
fn pull_brings_remote_changes_down() {
    if !rclone_available() {
        eprintln!("skipping: rclone not installed");
        return;
    }
    let w = World::new();
    w.seed("inbox", 2);
    assert_ok(&w.run(&["init"]), "init");

    // Something another machine added.
    std::fs::write(
        w.remote_of("docs").join("inbox/theirs.pdf"),
        b"from machine B",
    )
    .unwrap();

    assert_ok(&w.run(&["pull"]), "pull");
    assert!(w.local.join("inbox/theirs.pdf").exists());
}

#[test]
fn push_refuses_when_the_remote_has_changes() {
    if !rclone_available() {
        eprintln!("skipping: rclone not installed");
        return;
    }
    let w = World::new();
    w.seed("inbox", 2);
    assert_ok(&w.run(&["init"]), "init");

    std::fs::write(w.local.join("inbox/mine.pdf"), b"mine").unwrap();
    std::fs::write(w.remote_of("docs").join("inbox/theirs.pdf"), b"theirs").unwrap();

    let out = w.run(&["push"]);
    // Exit 12 is the documented directional-assertion contract.
    assert_eq!(out.status.code(), Some(12));
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("lode sync docs"),
        "must point at the escape hatch: {text}"
    );

    // Nothing was applied: the remote must not have received the local file.
    assert!(!w.remote_of("docs").join("inbox/mine.pdf").exists());
}

#[test]
fn pull_refuses_when_local_has_changes() {
    if !rclone_available() {
        eprintln!("skipping: rclone not installed");
        return;
    }
    let w = World::new();
    w.seed("inbox", 2);
    assert_ok(&w.run(&["init"]), "init");

    std::fs::write(w.local.join("inbox/mine.pdf"), b"mine").unwrap();

    let out = w.run(&["pull"]);
    assert_eq!(out.status.code(), Some(12));
    assert!(!w.remote_of("docs").join("inbox/mine.pdf").exists());
}

#[test]
fn dry_run_reports_the_plan_and_changes_nothing() {
    if !rclone_available() {
        eprintln!("skipping: rclone not installed");
        return;
    }
    let w = World::new();
    w.seed("inbox", 2);
    assert_ok(&w.run(&["init"]), "init");
    std::fs::write(w.local.join("inbox/new.pdf"), b"pending").unwrap();

    let out = w.run(&["sync", "--dry-run"]);
    assert_ok(&out, "dry run");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("new.pdf"), "{text}");
    assert!(text.contains("dry run"), "{text}");
    assert!(!w.remote_of("docs").join("inbox/new.pdf").exists());

    // The plan is still pending afterwards, i.e. the snapshot did not advance.
    assert!(w.stdout(&["status"]).contains("new.pdf"));
}

#[test]
fn the_delete_guard_blocks_apply_until_explicitly_overridden() {
    if !rclone_available() {
        eprintln!("skipping: rclone not installed");
        return;
    }
    let w = World::new();
    w.seed("inbox", 10);
    assert_ok(&w.run(&["init"]), "init");

    // Five genuine deletions — above the configured ceiling of 3.
    for i in 1..=5 {
        std::fs::remove_file(w.local.join(format!("inbox/doc{i}.pdf"))).unwrap();
    }

    let out = w.run(&["sync"]);
    assert_eq!(out.status.code(), Some(11));
    // Crucially, the remote still holds every file.
    assert_eq!(count_pdfs(&w.remote_of("docs")), 10);

    // The override is per-run and explicit.
    let out = w.run(&["sync", "--allow-deletes", "5"]);
    assert_ok(&out, "sync with override");
    assert_eq!(count_pdfs(&w.remote_of("docs")), 5);
}

#[test]
fn emptying_a_folder_entirely_is_refused_by_rclone() {
    if !rclone_available() {
        eprintln!("skipping: rclone not installed");
        return;
    }
    let w = World::new();
    w.seed("inbox", 4);
    assert_ok(&w.run(&["init"]), "init");
    std::fs::remove_dir_all(w.local.join("inbox")).unwrap();

    // rclone refuses to sync to an empty directory. This floor is its own, and --force
    // and --allow-deletes do not lift it — so even a deliberate override cannot wipe a
    // folder by accident.
    let out = w.run(&["sync", "--allow-deletes", "100"]);
    assert!(!out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("no files at all"), "{text}");
    assert_eq!(
        count_pdfs(&w.remote_of("docs")),
        4,
        "remote must be untouched"
    );
}

#[test]
fn a_conflict_blocks_the_apply_and_leaves_both_sides_untouched() {
    if !rclone_available() {
        eprintln!("skipping: rclone not installed");
        return;
    }
    let w = World::new();
    w.seed("inbox", 2);
    assert_ok(&w.run(&["init"]), "init");

    std::fs::write(w.local.join("inbox/doc1.pdf"), b"local edit").unwrap();
    std::fs::write(
        w.remote_of("docs").join("inbox/doc1.pdf"),
        b"remote edit, longer",
    )
    .unwrap();

    let out = w.run(&["sync"]);
    assert_eq!(out.status.code(), Some(10));

    // Neither side was modified, and no conflict-suffixed file was materialised.
    assert_eq!(
        std::fs::read(w.local.join("inbox/doc1.pdf")).unwrap(),
        b"local edit"
    );
    assert_eq!(
        std::fs::read(w.remote_of("docs").join("inbox/doc1.pdf")).unwrap(),
        b"remote edit, longer"
    );
}

#[test]
fn fanout_applies_clean_folders_and_skips_blocked_ones() {
    if !rclone_available() {
        eprintln!("skipping: rclone not installed");
        return;
    }
    let w = World::with_folders(&["docs", "notes"]);
    w.seed_in("docs", "inbox", 4);
    w.seed_in("notes", "inbox", 4);
    assert_ok(&w.run(&["init"]), "init");

    // `docs` gets an ordinary addition; `notes` gets a mass deletion that must be blocked.
    std::fs::write(w.local_of("docs").join("inbox/new.pdf"), b"fresh").unwrap();
    std::fs::remove_dir_all(w.local_of("notes").join("inbox")).unwrap();

    let out = w.run(&["sync"]);
    // One folder failing must not prevent the other from syncing...
    assert!(w.remote_of("docs").join("inbox/new.pdf").exists());
    // ...and the blocked one must be untouched.
    assert_eq!(count_pdfs(&w.remote_of("notes")), 4);

    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("skipped"), "{text}");
    // The overall run still reports the most specific failure.
    assert_eq!(out.status.code(), Some(11));
}

#[test]
fn unlock_clears_a_leftover_bisync_lock() {
    if !rclone_available() {
        eprintln!("skipping: rclone not installed");
        return;
    }
    let w = World::new();
    w.seed("inbox", 1);
    assert_ok(&w.run(&["init"]), "init");

    let workdir = w.root.join("cache/lode/bisync/docs");
    let lock = workdir.join("leftover.lck");
    std::fs::write(&lock, "pid 12345").unwrap();

    assert!(w.stdout(&["unlock", "docs"]).contains("cleared"));
    assert!(!lock.exists());
    assert!(w.stdout(&["unlock", "docs"]).contains("no lock held"));
}

// ---------------------------------------------------------------------------
// Filters
// ---------------------------------------------------------------------------

#[test]
fn os_junk_never_reaches_the_remote() {
    if !rclone_available() {
        eprintln!("skipping: rclone not installed");
        return;
    }
    let w = World::new();
    w.seed("inbox", 3);
    // Exactly what Finder leaves behind in every directory it opens.
    std::fs::write(w.local.join(".DS_Store"), b"finder junk").unwrap();
    std::fs::write(w.local.join("inbox/.DS_Store"), b"more finder junk").unwrap();
    std::fs::write(w.local.join("inbox/._doc1.pdf"), b"appledouble").unwrap();

    let out = w.run(&["init"]);
    assert_ok(&out, "init");
    // Only the three real documents are counted, not the junk.
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("3 file(s)"),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );

    assert!(!w.remote_of("docs").join(".DS_Store").exists());
    assert!(!w.remote_of("docs").join("inbox/.DS_Store").exists());
    assert!(!w.remote_of("docs").join("inbox/._doc1.pdf").exists());
}

#[test]
fn junk_appearing_later_does_not_register_as_a_change() {
    if !rclone_available() {
        eprintln!("skipping: rclone not installed");
        return;
    }
    let w = World::new();
    w.seed("inbox", 2);
    assert_ok(&w.run(&["init"]), "init");

    // Browsing the folder in Finder must not make the tool think there is work to do.
    std::fs::write(w.local.join("inbox/.DS_Store"), b"junk").unwrap();
    std::fs::write(w.local.join(".directory"), b"kde junk").unwrap();
    std::fs::write(w.remote_of("docs").join("Thumbs.db"), b"windows junk").unwrap();

    let out = w.run(&["status"]);
    assert_ok(&out, "status");
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("up to date"),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
}

#[test]
fn the_filter_fingerprint_is_recorded_in_the_snapshot() {
    if !rclone_available() {
        eprintln!("skipping: rclone not installed");
        return;
    }
    let w = World::new();
    w.seed("inbox", 1);
    assert_ok(&w.run(&["init"]), "init");

    let snap = w.root.join("state/lode/folders/docs/snapshot.json");
    let v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&snap).unwrap()).unwrap();
    let fp = v["filters"].as_str().unwrap();
    assert!(fp.starts_with("fnv1a64:"), "{v}");
    // doctor must report the same fingerprint, so a mismatch can be diagnosed by eye.
    assert!(w.stdout(&["doctor"]).contains(fp));
}

// ---------------------------------------------------------------------------
// add / forget
// ---------------------------------------------------------------------------

#[test]
fn add_configures_and_baselines_in_one_command() {
    if !rclone_available() {
        eprintln!("skipping: rclone not installed");
        return;
    }
    let w = World::empty();
    let local = w.root.join("papers");
    let remote = w.root.join("papers-remote");
    std::fs::create_dir_all(&local).unwrap();
    std::fs::create_dir_all(&remote).unwrap();
    std::fs::write(local.join("a.pdf"), b"first document").unwrap();

    let out = w.run(&[
        "add",
        "papers",
        "--local",
        local.to_str().unwrap(),
        "--remote",
        remote.to_str().unwrap(),
        "--max-deletes",
        "4",
    ]);
    assert_ok(&out, "add");

    // The config gained a stanza...
    let cfg = std::fs::read_to_string(&w.config).unwrap();
    assert!(cfg.contains("[folder.papers]"), "{cfg}");
    assert!(cfg.contains("max_deletes = 4"), "{cfg}");
    // ...the original comment survived the edit...
    assert!(cfg.contains("# empty"), "{cfg}");
    // ...and the baseline was established in the same command.
    assert!(w.stdout(&["folders"]).contains("ready"));
    assert!(remote.join("a.pdf").exists());
    assert!(w.stdout(&["status"]).contains("up to date"));
}

#[test]
fn add_refuses_a_duplicate_and_leaves_config_intact() {
    if !rclone_available() {
        eprintln!("skipping: rclone not installed");
        return;
    }
    let w = World::new();
    w.seed("inbox", 1);
    let out = w.run(&[
        "add",
        "docs",
        "--local",
        w.root.join("other").to_str().unwrap(),
        "--remote",
        w.root.join("other-remote").to_str().unwrap(),
    ]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("already configured"));
    // The pre-existing stanza is unchanged.
    let cfg = std::fs::read_to_string(&w.config).unwrap();
    assert!(cfg.contains(w.local.to_str().unwrap()), "{cfg}");
}

#[test]
fn add_no_init_writes_config_without_baselining() {
    if !rclone_available() {
        eprintln!("skipping: rclone not installed");
        return;
    }
    let w = World::empty();
    let local = w.root.join("later");
    let remote = w.root.join("later-remote");
    std::fs::create_dir_all(&remote).unwrap();

    let out = w.run(&[
        "add",
        "later",
        "--local",
        local.to_str().unwrap(),
        "--remote",
        remote.to_str().unwrap(),
        "--no-init",
    ]);
    assert_ok(&out, "add --no-init");
    assert!(String::from_utf8_lossy(&out.stdout).contains("lode init later"));
    assert!(w.stdout(&["folders"]).contains("not initialised"));
}

#[test]
fn forget_removes_config_and_state_but_never_files() {
    if !rclone_available() {
        eprintln!("skipping: rclone not installed");
        return;
    }
    let w = World::new();
    w.seed("inbox", 3);
    assert_ok(&w.run(&["init"]), "init");
    let snapshot = w.root.join("state/lode/folders/docs/snapshot.json");
    assert!(snapshot.exists());

    let out = w.run(&["forget", "docs"]);
    assert_ok(&out, "forget");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("no files were deleted"), "{text}");

    // Config entry and state are gone...
    assert!(!std::fs::read_to_string(&w.config)
        .unwrap()
        .contains("[folder.docs]"));
    assert!(!snapshot.exists());
    // ...but both sides of the data are untouched.
    assert_eq!(count_pdfs(&w.local), 3);
    assert_eq!(count_pdfs(&w.remote_of("docs")), 3);
}

#[test]
fn forget_keep_state_leaves_the_snapshot_behind() {
    if !rclone_available() {
        eprintln!("skipping: rclone not installed");
        return;
    }
    let w = World::new();
    w.seed("inbox", 2);
    assert_ok(&w.run(&["init"]), "init");
    let snapshot = w.root.join("state/lode/folders/docs/snapshot.json");

    assert_ok(
        &w.run(&["forget", "docs", "--keep-state"]),
        "forget --keep-state",
    );
    assert!(snapshot.exists(), "state should have been kept");
}

#[test]
fn forget_rejects_an_unknown_folder() {
    if !rclone_available() {
        eprintln!("skipping: rclone not installed");
        return;
    }
    let w = World::new();
    let out = w.run(&["forget", "nope"]);
    assert_eq!(out.status.code(), Some(2), "usage error");
    assert!(String::from_utf8_lossy(&out.stderr).contains("not configured"));
}

// ---------------------------------------------------------------------------
// Local trash
// ---------------------------------------------------------------------------

#[test]
fn a_file_deleted_elsewhere_is_recoverable_from_local_trash() {
    if !rclone_available() {
        eprintln!("skipping: rclone not installed");
        return;
    }
    let w = World::new();
    w.seed("inbox", 3);
    assert_ok(&w.run(&["init"]), "init");
    let original = std::fs::read(w.local.join("inbox/doc2.pdf")).unwrap();

    // Another machine deletes it; we pull the deletion down.
    std::fs::remove_file(w.remote_of("docs").join("inbox/doc2.pdf")).unwrap();
    assert_ok(&w.run(&["pull"]), "pull");
    assert!(
        !w.local.join("inbox/doc2.pdf").exists(),
        "deletion should propagate"
    );

    // It is not gone — it is in the trash, at its original relative path.
    let v = json(&w.run(&["trash", "list", "--json"]));
    assert_eq!(v.as_array().unwrap().len(), 1, "{v}");
    assert_eq!(v[0]["path"], "inbox/doc2.pdf", "{v}");
    assert_eq!(v[0]["folder"], "docs");
    assert!(v[0]["deleted_at"].as_str().unwrap().ends_with('Z'), "{v}");

    // And it comes back byte-identical.
    let out = w.run(&["trash", "restore", "docs", "inbox/doc2.pdf"]);
    assert_ok(&out, "restore");
    assert_eq!(
        std::fs::read(w.local.join("inbox/doc2.pdf")).unwrap(),
        original
    );

    // The restore is announced as a local change, and the backup is kept.
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("lode push docs"), "{text}");
    assert_eq!(
        json(&w.run(&["trash", "list", "--json"]))
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn an_overwritten_local_file_is_recoverable() {
    if !rclone_available() {
        eprintln!("skipping: rclone not installed");
        return;
    }
    let w = World::new();
    w.seed("inbox", 2);
    assert_ok(&w.run(&["init"]), "init");

    // The remote wins because the local side has no competing change — but the bytes we
    // had locally must still be recoverable.
    std::fs::write(
        w.remote_of("docs").join("inbox/doc1.pdf"),
        b"newer remote content",
    )
    .unwrap();
    assert_ok(&w.run(&["pull"]), "pull");
    assert_eq!(
        std::fs::read(w.local.join("inbox/doc1.pdf")).unwrap(),
        b"newer remote content"
    );

    let v = json(&w.run(&["trash", "list", "--json"]));
    assert_eq!(
        v[0]["path"], "inbox/doc1.pdf",
        "overwritten copy should be kept: {v}"
    );
}

#[test]
fn a_sync_that_destroys_nothing_leaves_no_trash_runs() {
    if !rclone_available() {
        eprintln!("skipping: rclone not installed");
        return;
    }
    let w = World::new();
    w.seed("inbox", 2);
    assert_ok(&w.run(&["init"]), "init");

    std::fs::write(w.local.join("inbox/new.pdf"), b"purely additive").unwrap();
    let out = w.run(&["push", "--json"]);
    assert_ok(&out, "push");
    assert_eq!(json(&out)[0]["trashed"], 0);

    // Empty run directories would make `trash list` useless noise.
    assert!(w.stdout(&["trash", "list"]).contains("trash is empty"));
    let trash_root = w.root.join("state/lode/trash/docs");
    let runs = std::fs::read_dir(&trash_root)
        .map(|rd| rd.count())
        .unwrap_or(0);
    assert_eq!(runs, 0, "no empty run dirs should remain");
}

#[test]
fn trash_prune_all_empties_the_trash() {
    if !rclone_available() {
        eprintln!("skipping: rclone not installed");
        return;
    }
    let w = World::new();
    w.seed("inbox", 2);
    assert_ok(&w.run(&["init"]), "init");
    std::fs::remove_file(w.remote_of("docs").join("inbox/doc1.pdf")).unwrap();
    assert_ok(&w.run(&["pull"]), "pull");
    assert_eq!(
        json(&w.run(&["trash", "list", "--json"]))
            .as_array()
            .unwrap()
            .len(),
        1
    );

    // Nothing is old enough for the default threshold.
    let out = w.run(&["trash", "prune"]);
    assert_ok(&out, "prune");
    assert!(String::from_utf8_lossy(&out.stdout).contains("nothing older than 30 day(s)"));
    assert_eq!(
        json(&w.run(&["trash", "list", "--json"]))
            .as_array()
            .unwrap()
            .len(),
        1
    );

    // --all is the explicit escape hatch.
    assert_ok(&w.run(&["trash", "prune", "--all"]), "prune --all");
    assert!(w.stdout(&["trash", "list"]).contains("trash is empty"));
}

#[test]
fn restoring_something_absent_fails_clearly() {
    if !rclone_available() {
        eprintln!("skipping: rclone not installed");
        return;
    }
    let w = World::new();
    w.seed("inbox", 1);
    assert_ok(&w.run(&["init"]), "init");
    let out = w.run(&["trash", "restore", "docs", "inbox/never-existed.pdf"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("not in the trash"));
}

// ---------------------------------------------------------------------------
// Cross-platform name hazards
// ---------------------------------------------------------------------------

#[test]
fn case_only_collisions_stop_the_sync() {
    if !rclone_available() {
        eprintln!("skipping: rclone not installed");
        return;
    }
    let w = World::new();
    w.seed("inbox", 2);
    assert_ok(&w.run(&["init"]), "init");

    // Two names a Mac cannot hold at once. On a case-insensitive filesystem the second
    // write would just overwrite the first, so create the pair on the remote side.
    std::fs::write(w.remote_of("docs").join("inbox/Report.pdf"), b"upper").unwrap();
    std::fs::write(w.remote_of("docs").join("inbox/report.pdf"), b"lower").unwrap();
    if !w.remote_of("docs").join("inbox/Report.pdf").exists()
        || std::fs::read(w.remote_of("docs").join("inbox/Report.pdf")).unwrap() == b"lower"
    {
        eprintln!("skipping: filesystem is case-insensitive, cannot stage the collision");
        return;
    }

    let out = w.run(&["sync"]);
    assert!(!out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("case collision"), "{text}");
    assert!(text.contains("Report.pdf"), "{text}");
    // Nothing was applied.
    assert!(!w.local.join("inbox/Report.pdf").exists());
}

#[test]
fn unicode_normalisation_collisions_stop_the_sync() {
    if !rclone_available() {
        eprintln!("skipping: rclone not installed");
        return;
    }
    let w = World::new();
    w.seed("inbox", 1);
    assert_ok(&w.run(&["init"]), "init");

    // The same visible name in both forms: composed, as Linux usually writes it, and
    // decomposed, as macOS does. Different bytes, one name.
    let nfc = "R\u{e9}sum\u{e9}.pdf";
    let nfd = "Re\u{301}sume\u{301}.pdf";
    std::fs::write(w.remote_of("docs").join(nfc), b"composed").unwrap();
    std::fs::write(w.remote_of("docs").join(nfd), b"decomposed").unwrap();
    if std::fs::read_dir(w.remote_of("docs"))
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().contains("sum"))
        .count()
        < 2
    {
        eprintln!("skipping: filesystem normalises filenames, cannot stage the collision");
        return;
    }

    let out = w.run(&["sync"]);
    assert!(!out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("normalisation collision"), "{text}");
    assert!(!w.local.join(nfc).exists() && !w.local.join(nfd).exists());
}

#[test]
fn doctor_reports_symlinks_and_case_sensitivity() {
    if !rclone_available() {
        eprintln!("skipping: rclone not installed");
        return;
    }
    let w = World::new();
    w.seed("inbox", 1);
    assert_ok(&w.run(&["init"]), "init");
    std::os::unix::fs::symlink(w.local.join("inbox/doc1.pdf"), w.local.join("shortcut.pdf"))
        .unwrap();

    let text = w.stdout(&["doctor"]);
    assert!(text.contains("docs: symlinks"), "{text}");
    assert!(text.contains("shortcut.pdf"), "{text}");
    assert!(text.contains("docs: filesystem"), "{text}");
    assert!(text.contains("docs: name hazards"), "{text}");
}

#[test]
fn an_ordinary_folder_reports_no_hazards() {
    if !rclone_available() {
        eprintln!("skipping: rclone not installed");
        return;
    }
    let w = World::new();
    w.seed("inbox", 3);
    assert_ok(&w.run(&["init"]), "init");
    let out = w.run(&["doctor"]);
    assert_ok(&out, "doctor");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("all checks passed"), "{text}");
    assert!(
        text.contains("docs: symlinks       none") || text.contains("symlinks"),
        "{text}"
    );
}

#[test]
fn forget_keeps_trashed_files_and_says_so() {
    if !rclone_available() {
        eprintln!("skipping: rclone not installed");
        return;
    }
    let w = World::new();
    w.seed("inbox", 3);
    assert_ok(&w.run(&["init"]), "init");
    // Put something in the trash: a file deleted elsewhere, now held only here.
    std::fs::remove_file(w.remote_of("docs").join("inbox/doc1.pdf")).unwrap();
    assert_ok(&w.run(&["pull"]), "pull");

    let out = w.run(&["forget", "docs"]);
    assert_ok(&out, "forget");
    let text = String::from_utf8_lossy(&out.stdout);
    // Silently discarding this would destroy the last copy of doc1.pdf.
    assert!(text.contains("KEPT 1 trashed file"), "{text}");
    assert!(text.contains("--purge-trash"), "{text}");
    let trashed = w.root.join("state/lode/trash/docs");
    assert!(trashed.exists(), "trash must survive forget");
}

#[test]
fn forget_purge_trash_removes_it_explicitly() {
    if !rclone_available() {
        eprintln!("skipping: rclone not installed");
        return;
    }
    let w = World::new();
    w.seed("inbox", 2);
    assert_ok(&w.run(&["init"]), "init");
    std::fs::remove_file(w.remote_of("docs").join("inbox/doc1.pdf")).unwrap();
    assert_ok(&w.run(&["pull"]), "pull");

    let out = w.run(&["forget", "docs", "--purge-trash"]);
    assert_ok(&out, "forget --purge-trash");
    assert!(String::from_utf8_lossy(&out.stdout).contains("purged trash"));
    assert!(!w.root.join("state/lode/trash/docs").exists());
}

#[test]
fn forget_leaves_no_empty_trash_directory() {
    if !rclone_available() {
        eprintln!("skipping: rclone not installed");
        return;
    }
    let w = World::new();
    w.seed("inbox", 2);
    assert_ok(&w.run(&["init"]), "init");
    let out = w.run(&["forget", "docs"]);
    assert_ok(&out, "forget");
    // Nothing was at risk, so no stray directory should be left behind.
    assert!(!String::from_utf8_lossy(&out.stdout).contains("KEPT"));
    assert!(!w.root.join("state/lode/trash/docs").exists());
}

#[test]
fn a_fresh_machine_downloads_an_existing_remote() {
    if !rclone_available() {
        eprintln!("skipping: rclone not installed");
        return;
    }
    // The onboarding case, and the opposite direction from every other test: the folder
    // already exists on the remote and the local side is empty. rclone's empty-side
    // refusal (which blocks an ordinary sync) does not apply to a resync, because that
    // check lives in the delta phase a resync skips.
    let w = World::new();
    let remote = w.remote_of("docs");
    std::fs::create_dir_all(remote.join("inbox")).unwrap();
    for i in 1..=8 {
        std::fs::write(remote.join(format!("inbox/doc{i}.pdf")), format!("doc {i}")).unwrap();
    }
    assert_eq!(count_files(&w.local), 0, "local must start empty");

    let out = w.run(&["init"]);
    assert_ok(&out, "init on a fresh machine");
    assert!(String::from_utf8_lossy(&out.stdout).contains("8 file(s)"));

    assert_eq!(
        count_files(&w.local),
        8,
        "the whole remote should have landed locally"
    );
    assert!(w.stdout(&["status"]).contains("up to date"));
}

fn count_files(dir: &Path) -> usize {
    let mut n = 0;
    let Ok(rd) = std::fs::read_dir(dir) else {
        return 0;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            n += count_files(&p);
        } else {
            n += 1;
        }
    }
    n
}

// ---------------------------------------------------------------------------
// Losing state
// ---------------------------------------------------------------------------

#[test]
fn a_lost_snapshot_blocks_init_instead_of_silently_resurrecting_files() {
    if !rclone_available() {
        eprintln!("skipping: rclone not installed");
        return;
    }
    let w = World::new();
    w.seed("inbox", 4);
    assert_ok(&w.run(&["init"]), "init");

    // A deletion made locally but never synced.
    std::fs::remove_file(w.local.join("inbox/doc2.pdf")).unwrap();
    // Now lose the merge base.
    std::fs::remove_file(w.root.join("state/lode/folders/docs/snapshot.json")).unwrap();

    // Plain `init` must refuse: resyncing would union both sides and bring doc2 back.
    let out = w.run(&["init"]);
    assert!(!out.status.success());
    let text = String::from_utf8_lossy(&out.stderr);
    assert!(
        text.contains("initialised on this machine before"),
        "{text}"
    );
    assert!(text.contains("lode resync docs --i-understand"), "{text}");
    assert!(
        !w.local.join("inbox/doc2.pdf").exists(),
        "nothing should have changed"
    );
}

#[test]
fn resync_requires_explicit_confirmation() {
    if !rclone_available() {
        eprintln!("skipping: rclone not installed");
        return;
    }
    let w = World::new();
    w.seed("inbox", 2);
    assert_ok(&w.run(&["init"]), "init");

    let out = w.run(&["resync", "docs"]);
    assert!(!out.status.success());
    let text = String::from_utf8_lossy(&out.stderr);
    assert!(text.contains("--i-understand"), "{text}");
    assert!(text.contains("unioning both sides"), "{text}");
}

#[test]
fn resync_re_establishes_a_baseline_after_the_snapshot_is_lost() {
    if !rclone_available() {
        eprintln!("skipping: rclone not installed");
        return;
    }
    let w = World::new();
    w.seed("inbox", 3);
    assert_ok(&w.run(&["init"]), "init");
    std::fs::remove_file(w.root.join("state/lode/folders/docs/snapshot.json")).unwrap();

    let out = w.run(&["resync", "docs", "--i-understand"]);
    assert_ok(&out, "resync");
    assert!(w.stdout(&["status"]).contains("up to date"));
    assert_eq!(count_files(&w.local), 3);
}

#[test]
fn a_regenerated_machine_id_is_diagnosed_as_such() {
    if !rclone_available() {
        eprintln!("skipping: rclone not installed");
        return;
    }
    let w = World::new();
    w.seed("inbox", 2);
    assert_ok(&w.run(&["init"]), "init");

    // Same host, new random suffix — what happens if machine.id is deleted and remade.
    let snap = w.root.join("state/lode/folders/docs/snapshot.json");
    let mut v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&snap).unwrap()).unwrap();
    let host = v["machine_id"]
        .as_str()
        .unwrap()
        .rsplit_once('-')
        .unwrap()
        .0
        .to_string();
    v["machine_id"] = serde_json::Value::String(format!("{host}-0000000000000000"));
    std::fs::write(&snap, serde_json::to_string_pretty(&v).unwrap()).unwrap();

    let text = w.stdout(&["status"]);
    // Must not be described as a snapshot from another machine — the cause is different
    // and so is the remedy.
    assert!(text.contains("machine's id has changed"), "{text}");
    assert!(!text.contains("never be synced between machines"), "{text}");
    assert!(text.contains("lode resync docs --i-understand"), "{text}");
}

#[test]
fn compare_works_without_a_snapshot() {
    if !rclone_available() {
        eprintln!("skipping: rclone not installed");
        return;
    }
    let w = World::new();
    w.seed("inbox", 3);
    assert_ok(&w.run(&["init"]), "init");

    // Diverge the sides, then destroy the merge base.
    std::fs::write(w.local.join("inbox/mine.pdf"), b"only local").unwrap();
    std::fs::write(w.remote_of("docs").join("inbox/theirs.pdf"), b"only remote").unwrap();
    std::fs::write(w.local.join("inbox/doc1.pdf"), b"local edit").unwrap();
    std::fs::write(
        w.remote_of("docs").join("inbox/doc1.pdf"),
        b"remote edit differs",
    )
    .unwrap();
    std::fs::remove_file(w.root.join("state/lode/folders/docs/snapshot.json")).unwrap();

    // `status` cannot answer without a merge base...
    assert!(w.stdout(&["status"]).contains("no snapshot yet"));

    // ...but `compare` still can, which is the whole point.
    let out = w.run(&["compare", "--json"]);
    assert_ok(&out, "compare");
    let v = json(&out);
    assert_eq!(v[0]["ok"], true, "{v}");
    assert_eq!(v[0]["local_only"][0], "inbox/mine.pdf", "{v}");
    assert_eq!(v[0]["remote_only"][0], "inbox/theirs.pdf", "{v}");
    assert_eq!(v[0]["differing"][0], "inbox/doc1.pdf", "{v}");
    assert_eq!(v[0]["in_sync"], false);
}

#[test]
fn resync_warns_which_side_wins_before_asking() {
    if !rclone_available() {
        eprintln!("skipping: rclone not installed");
        return;
    }
    let w = World::new();
    w.seed("inbox", 2);
    assert_ok(&w.run(&["init"]), "init");
    std::fs::write(w.local.join("inbox/doc1.pdf"), b"local wins this").unwrap();
    std::fs::write(
        w.remote_of("docs").join("inbox/doc1.pdf"),
        b"remote loses this",
    )
    .unwrap();

    let out = w.run(&["resync", "docs"]);
    assert!(
        !out.status.success(),
        "must not proceed without --i-understand"
    );
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("differ       inbox/doc1.pdf"), "{text}");
    assert!(text.contains("favour"), "{text}");
    assert!(text.contains("LOCAL copy"), "{text}");
    // Nothing changed.
    assert_eq!(
        std::fs::read(w.remote_of("docs").join("inbox/doc1.pdf")).unwrap(),
        b"remote loses this"
    );
}

#[test]
fn compare_reports_identical_sides() {
    if !rclone_available() {
        eprintln!("skipping: rclone not installed");
        return;
    }
    let w = World::new();
    w.seed("inbox", 4);
    assert_ok(&w.run(&["init"]), "init");
    let text = w.stdout(&["compare"]);
    assert!(text.contains("both sides identical (4 file(s))"), "{text}");
}
