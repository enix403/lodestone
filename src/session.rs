//! A session binds the resolved environment — rclone, this machine's identity — to the
//! two operations every command is built from: [`Session::plan`] and [`Session::apply`].
//!
//! Keeping both here means `status` and `sync`/`push`/`pull` cannot drift apart: the plan
//! a user previews is computed by exactly the same code that gates the mutation.

use crate::config::Folder;
use crate::error::{Error, Result};
use crate::plan::{Comparison, Plan};
use crate::rclone::bullet_list;
use crate::rclone::{Rclone, Version};
use crate::snapshot::{Listing, Snapshot};
use crate::{filters, hazards, machine, paths, timestamp, trash};

/// Substring rclone puts in the losing file's name when a conflict is materialised.
/// Derived from bisync's default `--conflict-suffix`.
const CONFLICT_MARKER: &str = ".conflict";

pub struct Session {
    pub rclone: Rclone,
    pub machine_id: String,
    pub version: Version,
}

/// What an apply actually did.
pub struct Applied {
    pub files: usize,
    /// Server-side renames, parsed from bisync's log. Reported because "12 moved, 0
    /// transferred" is the single most reassuring line this tool can print.
    pub moved: usize,
    pub copied: usize,
    pub deleted: usize,
    /// Conflict files rclone materialised between plan and apply. Should always be empty:
    /// the plan phase refuses to proceed when it sees a conflict. If it is not empty, the
    /// TOCTOU window was hit and the user must look.
    pub conflict_artifacts: Vec<String>,
    /// Trash run that caught anything removed or overwritten locally, if any was.
    pub trash_run: Option<String>,
    pub trashed: usize,
    pub log: String,
}

impl Session {
    pub fn new() -> Result<Self> {
        let rclone = Rclone::discover()?;
        let version = rclone.require_min_version()?;
        // The same filter file is used for listing and for syncing, so the plan and the
        // sync can never disagree about which files exist.
        let filter_file = filters::write_file(&paths::cache_dir())?;
        Ok(Self {
            rclone: rclone.with_filters(filter_file.display().to_string()),
            machine_id: machine::machine_id()?,
            version,
        })
    }

    /// The plan phase: three-way delta between the snapshot and both current listings.
    pub fn plan(&self, f: &Folder) -> Result<Plan> {
        let snapshot = Snapshot::load(&f.name, &self.machine_id)?;
        let local = self.local_listing(f)?;
        let remote = self.rclone.lsjson(&f.remote)?;
        check_collisions(&f.name, &local, &remote)?;
        Ok(Plan::compute(&f.name, &snapshot.entries, &local, &remote))
    }

    /// A two-way comparison of the sides, needing no snapshot.
    ///
    /// This is what remains answerable once the merge base is lost, and what you need in
    /// order to decide whether re-baselining is safe.
    pub fn compare(&self, f: &Folder) -> Result<Comparison> {
        let local = self.local_listing(f)?;
        let remote = self.rclone.lsjson(&f.remote)?;
        check_collisions(&f.name, &local, &remote)?;
        Ok(Comparison::compute(&local, &remote))
    }

    fn local_listing(&self, f: &Folder) -> Result<Listing> {
        if !f.local.exists() {
            return Err(Error::Config(format!(
                "local path {} does not exist",
                f.local.display()
            )));
        }
        self.rclone.lsjson(&f.local.display().to_string())
    }

    /// The apply phase. Only ever called with a plan that passed every gate.
    ///
    /// The snapshot is rewritten **only** on success. An interrupted or failed run
    /// therefore leaves the previous merge base intact, and the next plan simply sees the
    /// partially-applied state as ordinary changes — the design is self-healing here, so
    /// there is no half-updated snapshot to repair.
    pub fn apply(&self, f: &Folder) -> Result<Applied> {
        let workdir = paths::bisync_workdir(&f.name);
        std::fs::create_dir_all(&workdir).map_err(|e| Error::io(workdir.display(), e))?;

        // Anything bisync would destroy on the local side goes here instead. Only the
        // local side needs this: Drive keeps its own 30-day trash for the remote.
        let (run, trash_dir) = trash::begin_run(&f.name, timestamp::now_unix())?;
        let trash_arg = trash_dir.display().to_string();

        let log = self
            .rclone
            .bisync(
                &f.local.display().to_string(),
                &f.remote,
                &workdir.display().to_string(),
                &["-v", "--backup-dir1", &trash_arg],
            )
            .map_err(|e| {
                // Nothing was backed up if the run failed; do not leave an empty run behind.
                let _ = trash::discard_if_empty(&trash_dir);
                self.explain_bisync_failure(f, e)
            })?;

        let trashed = trash::list(&f.name)
            .into_iter()
            .filter(|e| e.run == run)
            .count();
        trash::discard_if_empty(&trash_dir)?;

        // Re-list after the fact rather than trusting the plan: bisync is the authority on
        // what actually landed.
        let entries = self.local_listing(f)?;
        let conflict_artifacts: Vec<String> = entries
            .keys()
            .filter(|p| p.contains(CONFLICT_MARKER))
            .cloned()
            .collect();
        let files = entries.len();
        Snapshot::new(&f.name, &self.machine_id, entries).save()?;

        Ok(Applied {
            files,
            moved: count_moves(&log),
            copied: count_transfers(&log),
            deleted: log.matches("Deleted").count(),
            conflict_artifacts,
            trash_run: (trashed > 0).then(|| run.clone()),
            trashed,
            log,
        })
    }

    /// Turn rclone's wall of text into something actionable for the failures we expect.
    fn explain_bisync_failure(&self, f: &Folder, e: Error) -> Error {
        let Error::RcloneFailed { code, stderr } = &e else {
            return e;
        };
        if stderr.contains("prior lock file found") {
            return Error::StaleLock(f.name.clone());
        }
        if stderr.contains("file name too long") {
            return Error::Config(format!(
                "bisync could not create its workdir files for {:?}: the flattened path pair \
                 exceeds the filename limit. Run `lode doctor` for the projected length.",
                f.name
            ));
        }
        // Must be checked before the --resync branch: rclone's empty-listing abort says
        // both things, and the empty-side cause is the actionable one.
        if stderr.contains("mpty current Path") {
            return Error::EmptySide(f.name.clone());
        }
        if stderr.contains("must run --resync") || stderr.contains("Must run --resync") {
            // The most likely reason is that lodestone's compiled-in filter set changed
            // between versions. Say so, instead of leaving the user to guess.
            if self.filters_changed(f) {
                return Error::FilterSetChanged(f.name.clone());
            }
            return Error::NeedsResync(f.name.clone());
        }
        Error::RcloneFailed {
            code: *code,
            stderr: stderr.clone(),
        }
    }

    /// Whether the active filter set differs from the one recorded in the snapshot.
    /// A snapshot predating filtering has an empty fingerprint and also counts as changed.
    fn filters_changed(&self, f: &Folder) -> bool {
        match Snapshot::load(&f.name, &self.machine_id) {
            Ok(s) => s.filters != filters::fingerprint(),
            // If the snapshot cannot be read we cannot claim filters are the cause.
            Err(_) => false,
        }
    }
}

/// Files bisync actually transferred, from its log.
///
/// rclone words this several ways — `Copied (new)` for a fresh file, `Copied (replaced
/// existing)` for an overwrite, `Copied (server-side copy)` when the backend did it —
/// so matching only the first undercounts and reports "0 transferred" for a run that
/// plainly uploaded something. Matching the common prefix covers all of them.
pub(crate) fn count_transfers(log: &str) -> usize {
    log.matches("Copied (").count()
}

/// Files collapsed into server-side renames rather than re-transferred.
pub(crate) fn count_moves(log: &str) -> usize {
    log.matches("Moved (server-side)").count()
}

/// Refuse to plan when either side holds paths some filesystem in the fleet cannot tell
/// apart. Checked across the union of both sides, so a name created as NFC on Linux and as
/// NFD on macOS is caught even though neither side alone looks wrong.
pub(crate) fn check_collisions(folder: &str, local: &Listing, remote: &Listing) -> Result<()> {
    let mut paths: Vec<String> = local.keys().chain(remote.keys()).cloned().collect();
    paths.sort();
    paths.dedup();

    let found = hazards::name_collisions(&paths);
    if !found.is_empty() {
        // Label the whole report by the broadest cause present, so the message names the
        // problem the user is most likely to hit first.
        let kind = if found.iter().any(|c| c.kind == hazards::CollisionKind::Case) {
            hazards::CollisionKind::Case
        } else {
            hazards::CollisionKind::Normalisation
        };
        let detail = found
            .iter()
            .map(|c| format!("  [{}]\n{}", c.kind.label(), bullet_list(&c.paths)))
            .collect::<Vec<_>>()
            .join("\n");
        return Err(Error::NameCollisions {
            folder: folder.to_string(),
            kind: kind.label(),
            count: found.len(),
            detail,
        });
    }
    Ok(())
}

/// Remove bisync's lock files for a folder, after a run was killed mid-flight.
///
/// Deliberately a separate, explicit command: a lock that clears itself is not a lock.
pub fn clear_lock(folder: &str) -> Result<Vec<String>> {
    let workdir = paths::bisync_workdir(folder);
    let mut removed = Vec::new();
    let Ok(entries) = std::fs::read_dir(&workdir) else {
        return Ok(removed);
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "lck") {
            std::fs::remove_file(&path).map_err(|e| Error::io(path.display(), e))?;
            removed.push(path.display().to_string());
        }
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::Entry;

    fn listing(paths: &[&str]) -> Listing {
        paths
            .iter()
            .enumerate()
            .map(|(i, p)| {
                (
                    p.to_string(),
                    Entry::new(10 + i as u64, "t", Some(format!("h{i}"))),
                )
            })
            .collect()
    }

    #[test]
    fn transfer_counting_covers_every_wording_rclone_uses() {
        // Observed forms. Counting only "Copied (new)" made a push that overwrote a file
        // report "0 transferred", which is actively misleading.
        let log = "\
INFO  : inbox/a.pdf: Copied (new)
INFO  : inbox/b.pdf: Copied (replaced existing)
INFO  : inbox/c.pdf: Copied (server-side copy)
INFO  : inbox/d.pdf: Moved (server-side) to: archive/d.pdf
INFO  : archive/d.pdf: Renamed from \"inbox/d.pdf\"
INFO  : inbox/e.pdf: Deleted
";
        assert_eq!(count_transfers(log), 3);
        assert_eq!(count_moves(log), 1, "a move is not a transfer");
    }

    #[test]
    fn counting_an_empty_log_yields_zero() {
        assert_eq!(count_transfers(""), 0);
        assert_eq!(count_moves(""), 0);
    }

    #[test]
    fn a_clean_pair_of_listings_passes() {
        let l = listing(&["inbox/a.pdf", "inbox/b.pdf"]);
        let r = listing(&["inbox/a.pdf", "archive/c.pdf"]);
        assert!(check_collisions("docs", &l, &r).is_ok());
    }

    #[test]
    fn a_case_collision_within_one_side_is_refused() {
        let l = listing(&["inbox/Report.pdf", "inbox/report.pdf"]);
        let r = Listing::new();
        let err = check_collisions("docs", &l, &r).unwrap_err();
        match &err {
            Error::NameCollisions { kind, count, .. } => {
                assert_eq!(*kind, "case");
                assert_eq!(*count, 1);
            }
            other => panic!("expected NameCollisions, got {other:?}"),
        }
        assert!(err.to_string().contains("Report.pdf"), "{err}");
    }

    #[test]
    fn a_collision_spanning_the_two_sides_is_refused() {
        // Neither side alone looks wrong: the Mac holds the decomposed name, Linux wrote
        // the composed one. Only the union reveals the problem, which is why the check
        // runs over both.
        let l = listing(&["Re\u{301}sume\u{301}.pdf"]);
        let r = listing(&["R\u{e9}sum\u{e9}.pdf"]);
        assert!(
            check_collisions("docs", &l, &l).is_ok(),
            "one side alone is fine"
        );
        assert!(
            check_collisions("docs", &r, &r).is_ok(),
            "one side alone is fine"
        );

        let err = check_collisions("docs", &l, &r).unwrap_err();
        assert!(
            matches!(&err, Error::NameCollisions { kind, .. } if *kind == "unicode normalisation"),
            "a pure NFC/NFD pair must be labelled as normalisation, not case: {err:?}"
        );
    }

    #[test]
    fn the_same_path_present_on_both_sides_is_not_a_collision() {
        // The overwhelmingly common case: both sides hold the identical path. Deduping the
        // union before grouping is what keeps this from being reported as a collision.
        let l = listing(&["inbox/a.pdf", "inbox/b.pdf"]);
        assert!(check_collisions("docs", &l, &l).is_ok());
    }

    #[test]
    fn case_is_reported_in_preference_to_normalisation() {
        // A pair differing in both should be described once, as the broader problem.
        let l = listing(&["Re\u{301}sume\u{301}.pdf", "r\u{e9}sum\u{e9}.pdf"]);
        let err = check_collisions("docs", &l, &Listing::new()).unwrap_err();
        assert!(
            matches!(&err, Error::NameCollisions { kind, count, .. } if *kind == "case" && *count == 1),
            "{err:?}"
        );
    }
}
