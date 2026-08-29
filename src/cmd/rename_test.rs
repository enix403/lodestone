//! `lode doctor rename-test` — the empirical rename harness.
//!
//! This exists because the whole efficiency story rests on one question that no amount
//! of reading documentation settles: **does this rclone collapse a moved subtree into
//! server-side renames, or does it re-upload every file?**
//!
//! The harness answers it locally, with no network and no cloud account: two scratch
//! directories are a perfectly valid bisync pair. It also demonstrates *why* lodestone
//! passes `--force`, by running the same scenario both ways.

use lodestone::error::ExitCode;
use lodestone::rclone::FILENAME_LIMIT;
use lodestone::{paths, rclone::Rclone, Error, Result};
use std::io::Write;
use std::path::{Path, PathBuf};

const FILES: usize = 12;

pub fn run() -> Result<ExitCode> {
    let rclone = Rclone::discover()?;
    let version = rclone.require_min_version()?;
    println!("rclone {version}");

    let root = scratch_dir()?;
    let p1 = root.join("path1");
    let p2 = root.join("path2");
    let wd = root.join("workdir");
    for d in [&p1, &p2, &wd] {
        std::fs::create_dir_all(d).map_err(|e| Error::io(d.display(), e))?;
    }
    println!("scratch: {}", root.display());

    // 1. A tree of distinct files, baselined so both sides agree.
    let inbox = p1.join("inbox");
    std::fs::create_dir_all(&inbox).map_err(|e| Error::io(inbox.display(), e))?;
    for i in 1..=FILES {
        write_file(&inbox.join(format!("doc{i}.pdf")), i)?;
    }
    let (p1s, p2s, wds) = (
        p1.display().to_string(),
        p2.display().to_string(),
        wd.display().to_string(),
    );
    rclone.bisync(&p1s, &p2s, &wds, &["--resync"])?;
    println!("baseline: {FILES} file(s) on both sides");

    // 2. Move the whole subtree — the reorganisation this tool is built around.
    let archive = p1.join("archive");
    std::fs::create_dir_all(&archive).map_err(|e| Error::io(archive.display(), e))?;
    let dest = archive.join("2024");
    std::fs::rename(&inbox, &dest).map_err(|e| Error::io(dest.display(), e))?;
    println!("moved inbox/ -> archive/2024/ ({FILES} files)");

    // 3. Control: rclone's own percentage guard, with --track-renames but without
    //    --force. Expected to abort, because the guard runs during delta detection,
    //    before the sync stage where rename tracking would apply.
    let control = std::process::Command::new(&rclone.binary)
        .args([
            "bisync",
            "--workdir",
            &wds,
            "--track-renames",
            "--track-renames-strategy",
            "hash",
            "--stats",
            "0",
            "-v",
            &p1s,
            &p2s,
        ])
        .output()
        .map_err(|e| Error::io("rclone", e))?;
    let control_out = format!(
        "{}{}",
        String::from_utf8_lossy(&control.stdout),
        String::from_utf8_lossy(&control.stderr)
    );
    let guard_fired = control_out.contains("too many deletes");
    println!(
        "\nwithout --force: {}",
        if guard_fired {
            "rclone's percentage guard aborted the run (expected)"
        } else {
            "run was not blocked"
        }
    );

    // 4. The real question: with lodestone's flags, are these moves or re-uploads?
    let out = rclone.bisync(&p1s, &p2s, &wds, &["-v"])?;
    let moved = out.matches("Moved (server-side)").count();
    let copied = out.matches("Copied (new)").count();

    println!("with lodestone's flags: {moved} server-side move(s), {copied} copy/copies");

    let landed = count_files(&p2.join("archive").join("2024"));
    let left_behind = count_files(&p2.join("inbox"));

    println!("\nresult");
    println!("  files at the new path on path2 : {landed}/{FILES}");
    println!("  files left at the old path     : {left_behind}");

    let pass = moved == FILES && copied == 0 && landed == FILES && left_behind == 0;
    if pass {
        println!("\nPASS — moves are tracked as server-side renames; no data is re-transferred.");
        println!("Note: this requires --force, which disables rclone's own delete guard.");
        println!("lodestone therefore applies its own guard in the plan phase, counting only");
        println!("deletes whose content does not reappear elsewhere.");
        Ok(ExitCode::Ok)
    } else {
        println!("\nFAIL — this rclone did not collapse the move into server-side renames.");
        println!("Reorganisations would re-upload every affected file. See docs/TDD.md");
        println!("§Rename detection for the fallback (applying moves via `rclone moveto`).");
        Ok(ExitCode::Unexpected)
    }
}

/// Deliberately short: bisync encodes *both* full paths into a single workdir filename,
/// so a deep scratch path can breach the 255-byte filename limit and fail with
/// "file name too long" before the test even starts. See `Rclone::session_name_len`.
fn scratch_dir() -> Result<PathBuf> {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let short = format!("{:x}", stamp & 0xff_ffff);
    let p = paths::cache_dir().join("rt").join(short);
    std::fs::create_dir_all(&p).map_err(|e| Error::io(p.display(), e))?;

    let projected = crate_session_len(&p);
    if projected > FILENAME_LIMIT {
        return Err(Error::Config(format!(
            "scratch path {} is too deep: bisync would need a {projected}-byte workdir \
             filename but the limit is {FILENAME_LIMIT}. Set XDG_CACHE_HOME to a shorter path.",
            p.display()
        )));
    }
    Ok(p)
}

fn crate_session_len(root: &Path) -> usize {
    lodestone::rclone::session_name_len(
        &root.join("path1").display().to_string(),
        &root.join("path2").display().to_string(),
    )
}

/// Distinct content per file so hash-based rename matching has something to match on.
fn write_file(path: &Path, seed: usize) -> Result<()> {
    let mut f = std::fs::File::create(path).map_err(|e| Error::io(path.display(), e))?;
    let body: Vec<u8> = (0..2048u32)
        .map(|i| (i.wrapping_mul(seed as u32).wrapping_add(seed as u32) % 251) as u8)
        .collect();
    f.write_all(&body).map_err(|e| Error::io(path.display(), e))
}

fn count_files(dir: &Path) -> usize {
    std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter(|e| e.path().is_file())
                .count()
        })
        .unwrap_or(0)
}
