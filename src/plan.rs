//! The plan phase.
//!
//! lodestone computes the plan itself rather than scraping rclone's output: a three-way
//! delta between the stored snapshot (the merge base), the current local listing, and
//! the current remote listing. rclone is the executor, not the planner.
//!
//! The rule that makes the delete guard usable across machines:
//!
//! > A delete whose content reappears elsewhere is a **move**. Only a delete whose
//! > content vanishes entirely counts against the guard.
//!
//! This is applied symmetrically to both sides, which is what stops machine B from
//! tripping its delete guard every time machine A reorganises a folder — machine B has
//! no local inode information to appeal to, only hashes, and hashes are enough.

use crate::error::{Error, Result};
use crate::snapshot::{Entry, Listing};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// `lode sync` — no directional assertion.
    Both,
    /// `lode push` — asserts the remote has no changes to bring in.
    Push,
    /// `lode pull` — asserts the local side has no changes to send out.
    Pull,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rename {
    pub from: String,
    pub to: String,
}

/// What changed on one side, relative to the snapshot.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SideDelta {
    pub added: Listing,
    pub modified: Listing,
    pub deleted: Listing,
    /// Populated by [`SideDelta::extract_renames`], which moves matched pairs out of
    /// `added` and `deleted`.
    pub renames: Vec<Rename>,
}

impl SideDelta {
    /// Diff a current listing against the snapshot.
    pub fn compute(base: &Listing, current: &Listing) -> Self {
        let mut d = SideDelta::default();
        for (path, entry) in current {
            match base.get(path) {
                None => {
                    d.added.insert(path.clone(), entry.clone());
                }
                Some(old) => {
                    // Unknown-content (no hash on either side) falls back to size+modtime,
                    // which is what rclone's own default `--compare size,modtime` does.
                    let changed = match old.same_content(entry) {
                        Some(same) => !same,
                        None => old.size != entry.size || old.modtime != entry.modtime,
                    };
                    if changed {
                        d.modified.insert(path.clone(), entry.clone());
                    }
                }
            }
        }
        for (path, entry) in base {
            if !current.contains_key(path) {
                d.deleted.insert(path.clone(), entry.clone());
            }
        }
        d
    }

    /// Collapse delete+create pairs with identical content into renames.
    ///
    /// Matching is on `(size, hash)`. Entries without a hash cannot be matched and stay
    /// as a delete plus a create — deliberately conservative, since a false rename would
    /// let a real delete slip past the guard.
    pub fn extract_renames(&mut self) {
        // Group deletion candidates by content. Several files may share content (exact
        // duplicates), so each key holds a queue; sorted key order plus FIFO within a key
        // makes the pairing deterministic.
        let mut by_content: BTreeMap<(u64, String), Vec<String>> = BTreeMap::new();
        for (path, entry) in &self.deleted {
            if let Some(hash) = &entry.hash {
                by_content
                    .entry((entry.size, hash.clone()))
                    .or_default()
                    .push(path.clone());
            }
        }

        let mut renames = Vec::new();
        let mut consumed_adds = Vec::new();
        for (path, entry) in &self.added {
            let Some(hash) = &entry.hash else { continue };
            let key = (entry.size, hash.clone());
            let Some(queue) = by_content.get_mut(&key) else {
                continue;
            };
            if queue.is_empty() {
                continue;
            }
            let from = queue.remove(0);
            renames.push(Rename {
                from,
                to: path.clone(),
            });
            consumed_adds.push(path.clone());
        }

        for path in consumed_adds {
            self.added.remove(&path);
        }
        for r in &renames {
            self.deleted.remove(&r.from);
        }
        renames.sort_by(|a, b| a.to.cmp(&b.to));
        self.renames = renames;
    }

    /// Deletes remaining after rename extraction: content that vanished entirely.
    pub fn true_deletes(&self) -> usize {
        self.deleted.len()
    }

    pub fn is_empty(&self) -> bool {
        self.added.is_empty()
            && self.modified.is_empty()
            && self.deleted.is_empty()
            && self.renames.is_empty()
    }

    /// Every path this side touched, used for conflict detection.
    fn touched(&self) -> BTreeMap<&String, ChangeKind> {
        let mut m = BTreeMap::new();
        for p in self.added.keys() {
            m.insert(p, ChangeKind::Added);
        }
        for p in self.modified.keys() {
            m.insert(p, ChangeKind::Modified);
        }
        for p in self.deleted.keys() {
            m.insert(p, ChangeKind::Deleted);
        }
        m
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChangeKind {
    Added,
    Modified,
    Deleted,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConflictKind {
    /// Both sides changed the file to different content.
    BothEdited,
    /// One side edited it, the other deleted it.
    EditedAndDeleted { deleted_on_local: bool },
    /// Both sides created the same path with different content.
    BothCreated,
    /// Both sides changed it, but no hash is available so we cannot prove convergence.
    /// Treated as a conflict: unknown must fail safe.
    Indeterminate,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conflict {
    pub path: String,
    pub kind: ConflictKind,
}

/// A snapshot-free, two-way comparison of the sides.
///
/// The plan phase needs a merge base to say *what happened*. When the snapshot is gone
/// that question is unanswerable — but "how do the two sides differ right now?" still is,
/// and it is what you need in order to decide whether re-baselining is safe.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Comparison {
    /// Present locally, absent on the remote.
    pub local_only: Vec<String>,
    /// Present on the remote, absent locally.
    pub remote_only: Vec<String>,
    /// Present on both with different content. A resync resolves these in favour of the
    /// local side, overwriting the remote copy.
    pub differing: Vec<String>,
    /// Present on both, identical.
    pub identical: usize,
}

impl Comparison {
    pub fn compute(local: &Listing, remote: &Listing) -> Self {
        let mut c = Comparison::default();
        for (path, l) in local {
            match remote.get(path) {
                None => c.local_only.push(path.clone()),
                Some(r) => match l.same_content(r) {
                    Some(true) => c.identical += 1,
                    // Unknown content (no hash either side) is reported as differing:
                    // claiming they match without evidence is the unsafe direction.
                    _ => c.differing.push(path.clone()),
                },
            }
        }
        for path in remote.keys() {
            if !local.contains_key(path) {
                c.remote_only.push(path.clone());
            }
        }
        c
    }

    pub fn in_sync(&self) -> bool {
        self.local_only.is_empty() && self.remote_only.is_empty() && self.differing.is_empty()
    }
}

#[derive(Debug, Clone)]
pub struct Plan {
    pub folder: String,
    pub local: SideDelta,
    pub remote: SideDelta,
    pub conflicts: Vec<Conflict>,
}

impl Plan {
    /// Build a plan from the merge base and both current listings.
    pub fn compute(folder: &str, base: &Listing, local: &Listing, remote: &Listing) -> Self {
        let mut l = SideDelta::compute(base, local);
        let mut r = SideDelta::compute(base, remote);

        // Conflicts are detected on the *raw* deltas, before renames are extracted.
        // Rename extraction is an interpretation layered on top for the guard's benefit;
        // conflict safety must not depend on it.
        let conflicts = detect_conflicts(&l, &r);

        l.extract_renames();
        r.extract_renames();

        Plan {
            folder: folder.to_string(),
            local: l,
            remote: r,
            conflicts,
        }
    }

    pub fn is_clean(&self) -> bool {
        self.local.is_empty() && self.remote.is_empty()
    }

    /// Apply every gate. Returns `Ok(())` only if the plan is safe to execute.
    ///
    /// `max_deletes` is the true-delete ceiling; `lode sync --allow-deletes N` raises it
    /// for a single run.
    pub fn evaluate(&self, max_deletes: usize, dir: Direction) -> Result<()> {
        if !self.conflicts.is_empty() {
            return Err(Error::Conflicts(self.conflicts.len()));
        }

        // Guard both directions independently: a local delete destroys data on the
        // remote, a remote delete destroys it locally. Either is worth stopping for.
        for (side, delta) in [("local", &self.local), ("remote", &self.remote)] {
            if delta.true_deletes() > max_deletes {
                return Err(Error::DeleteGuard {
                    side: side.to_string(),
                    found: delta.true_deletes(),
                    limit: max_deletes,
                });
            }
        }

        match dir {
            Direction::Both => {}
            Direction::Push if !self.remote.is_empty() => {
                return Err(Error::Assertion(format!(
                    "`push` requires no incoming remote changes, but the remote has {}. \
                     Run `lode sync {}` instead.",
                    describe(&self.remote),
                    self.folder
                )))
            }
            Direction::Pull if !self.local.is_empty() => {
                return Err(Error::Assertion(format!(
                    "`pull` requires no outgoing local changes, but the local side has {}. \
                     Run `lode sync {}` instead.",
                    describe(&self.local),
                    self.folder
                )))
            }
            _ => {}
        }
        Ok(())
    }

    /// One-line summary, e.g. `↑ 4 new  ↻ 12 renamed  ↓ 2 incoming  ✗ 1 conflict`.
    pub fn summary(&self) -> String {
        if self.is_clean() && self.conflicts.is_empty() {
            return "up to date".to_string();
        }
        let mut parts = Vec::new();
        let up = self.local.added.len() + self.local.modified.len();
        let down = self.remote.added.len() + self.remote.modified.len();
        let renamed = self.local.renames.len() + self.remote.renames.len();
        let deleted = self.local.true_deletes() + self.remote.true_deletes();
        if up > 0 {
            parts.push(format!("\u{2191} {up} outgoing"));
        }
        if down > 0 {
            parts.push(format!("\u{2193} {down} incoming"));
        }
        if renamed > 0 {
            parts.push(format!("\u{21bb} {renamed} renamed"));
        }
        if deleted > 0 {
            parts.push(format!("\u{2212} {deleted} deleted"));
        }
        if !self.conflicts.is_empty() {
            parts.push(format!("\u{2717} {} conflict(s)", self.conflicts.len()));
        }
        parts.join("  ")
    }
}

fn describe(d: &SideDelta) -> String {
    let mut parts = Vec::new();
    if !d.added.is_empty() {
        parts.push(format!("{} new", d.added.len()));
    }
    if !d.modified.is_empty() {
        parts.push(format!("{} modified", d.modified.len()));
    }
    if !d.renames.is_empty() {
        parts.push(format!("{} renamed", d.renames.len()));
    }
    if !d.deleted.is_empty() {
        parts.push(format!("{} deleted", d.deleted.len()));
    }
    if parts.is_empty() {
        "no changes".into()
    } else {
        parts.join(", ")
    }
}

fn detect_conflicts(local: &SideDelta, remote: &SideDelta) -> Vec<Conflict> {
    let lt = local.touched();
    let rt = remote.touched();
    let mut out = Vec::new();

    for (path, lk) in &lt {
        let Some(rk) = rt.get(*path) else { continue };
        let kind = match (lk, rk) {
            // Both removed it: the sides agree. Nothing to resolve.
            (ChangeKind::Deleted, ChangeKind::Deleted) => continue,

            (ChangeKind::Deleted, _) => ConflictKind::EditedAndDeleted {
                deleted_on_local: true,
            },
            (_, ChangeKind::Deleted) => ConflictKind::EditedAndDeleted {
                deleted_on_local: false,
            },

            // Both sides produced content at this path. If it is byte-identical they
            // converged independently and there is nothing to resolve.
            (a, b) => {
                let le = pick(local, path, *a);
                let re = pick(remote, path, *b);
                match (le, re) {
                    (Some(le), Some(re)) => match le.same_content(re) {
                        Some(true) => continue,
                        Some(false) if *a == ChangeKind::Added => ConflictKind::BothCreated,
                        Some(false) => ConflictKind::BothEdited,
                        None => ConflictKind::Indeterminate,
                    },
                    _ => ConflictKind::Indeterminate,
                }
            }
        };
        out.push(Conflict {
            path: (*path).clone(),
            kind,
        });
    }
    out
}

fn pick<'a>(d: &'a SideDelta, path: &str, kind: ChangeKind) -> Option<&'a Entry> {
    match kind {
        ChangeKind::Added => d.added.get(path),
        ChangeKind::Modified => d.modified.get(path),
        ChangeKind::Deleted => d.deleted.get(path),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(size: u64, hash: &str) -> Entry {
        Entry::new(size, "2026-01-01T00:00:00Z", Some(hash.to_string()))
    }
    fn nohash(size: u64, modtime: &str) -> Entry {
        Entry::new(size, modtime, None)
    }
    fn listing(items: &[(&str, Entry)]) -> Listing {
        items
            .iter()
            .map(|(p, e)| (p.to_string(), e.clone()))
            .collect()
    }

    #[test]
    fn detects_add_modify_delete() {
        let base = listing(&[("a", e(1, "h1")), ("b", e(2, "h2"))]);
        let cur = listing(&[("a", e(1, "h1")), ("b", e(2, "CHANGED")), ("c", e(3, "h3"))]);
        let d = SideDelta::compute(&base, &cur);
        assert_eq!(d.added.keys().collect::<Vec<_>>(), ["c"]);
        assert_eq!(d.modified.keys().collect::<Vec<_>>(), ["b"]);
        assert!(d.deleted.is_empty());
    }

    #[test]
    fn a_moved_subtree_is_renames_not_deletes() {
        // The scenario from the design discussion: 12 PDFs moved from inbox/ to
        // archive/2024/. This must read as 12 renames and *zero* true deletes.
        let base: Listing = (1..=12)
            .map(|i| (format!("inbox/doc{i}.pdf"), e(20000 + i, &format!("h{i}"))))
            .collect();
        let cur: Listing = (1..=12)
            .map(|i| {
                (
                    format!("archive/2024/doc{i}.pdf"),
                    e(20000 + i, &format!("h{i}")),
                )
            })
            .collect();

        let mut d = SideDelta::compute(&base, &cur);
        assert_eq!(
            d.deleted.len(),
            12,
            "before extraction these look like deletes"
        );
        assert_eq!(d.added.len(), 12);

        d.extract_renames();
        assert_eq!(d.renames.len(), 12);
        assert_eq!(d.true_deletes(), 0, "a move must never count as a delete");
        assert!(d.added.is_empty());
        assert_eq!(
            d.renames[0],
            Rename {
                from: "inbox/doc1.pdf".into(),
                to: "archive/2024/doc1.pdf".into()
            }
        );
    }

    #[test]
    fn machine_b_sees_the_same_reorg_as_renames() {
        // The hole this design closes: machine B has no inode information at all, only
        // hashes from the remote listing. It must still classify A's reorg as renames.
        let base: Listing = (1..=300)
            .map(|i| (format!("inbox/doc{i}.pdf"), e(1000 + i, &format!("h{i}"))))
            .collect();
        let remote_now: Listing = (1..=300)
            .map(|i| (format!("archive/doc{i}.pdf"), e(1000 + i, &format!("h{i}"))))
            .collect();

        let plan = Plan::compute("silvermine", &base, &base, &remote_now);
        assert_eq!(plan.remote.renames.len(), 300);
        assert_eq!(plan.remote.true_deletes(), 0);
        // With a strict ceiling of 10, this must still pass.
        assert!(plan.evaluate(10, Direction::Both).is_ok());
    }

    #[test]
    fn a_real_deletion_still_counts() {
        let base = listing(&[("a", e(1, "h1")), ("b", e(2, "h2")), ("c", e(3, "h3"))]);
        let cur = listing(&[("a", e(1, "h1"))]);
        let mut d = SideDelta::compute(&base, &cur);
        d.extract_renames();
        assert_eq!(d.true_deletes(), 2);
        assert!(d.renames.is_empty());
    }

    #[test]
    fn delete_guard_trips_and_names_the_side() {
        let base: Listing = (1..=50)
            .map(|i| (format!("f{i}"), e(i, &format!("h{i}"))))
            .collect();
        let empty = Listing::new();
        // Local side wiped — the "folder didn't mount" catastrophe.
        let plan = Plan::compute("f", &base, &empty, &base);
        let err = plan.evaluate(10, Direction::Both).unwrap_err();
        match err {
            Error::DeleteGuard { side, found, limit } => {
                assert_eq!(side, "local");
                assert_eq!(found, 50);
                assert_eq!(limit, 10);
            }
            other => panic!("expected DeleteGuard, got {other:?}"),
        }
        // An explicit override raises the ceiling for one run.
        assert!(plan.evaluate(100, Direction::Both).is_ok());
    }

    #[test]
    fn rename_plus_content_change_reads_as_delete_plus_add() {
        // Accepted limitation: renaming *and* re-exporting a file cannot be matched.
        let base = listing(&[("old.pdf", e(100, "h1"))]);
        let cur = listing(&[("new.pdf", e(120, "h2"))]);
        let mut d = SideDelta::compute(&base, &cur);
        d.extract_renames();
        assert_eq!(d.true_deletes(), 1);
        assert_eq!(d.added.len(), 1);
        assert!(d.renames.is_empty());
    }

    #[test]
    fn duplicate_content_pairs_one_to_one() {
        // Two identical files both moved: must produce exactly two renames, not four.
        let base = listing(&[("a/1.pdf", e(10, "same")), ("a/2.pdf", e(10, "same"))]);
        let cur = listing(&[("b/1.pdf", e(10, "same")), ("b/2.pdf", e(10, "same"))]);
        let mut d = SideDelta::compute(&base, &cur);
        d.extract_renames();
        assert_eq!(d.renames.len(), 2);
        assert_eq!(d.true_deletes(), 0);
        assert!(d.added.is_empty());
    }

    #[test]
    fn unmatched_duplicate_leaves_a_real_delete() {
        // Two identical files, only one survives the move: one rename, one true delete.
        let base = listing(&[("a/1.pdf", e(10, "same")), ("a/2.pdf", e(10, "same"))]);
        let cur = listing(&[("b/1.pdf", e(10, "same"))]);
        let mut d = SideDelta::compute(&base, &cur);
        d.extract_renames();
        assert_eq!(d.renames.len(), 1);
        assert_eq!(d.true_deletes(), 1);
    }

    #[test]
    fn hashless_entries_are_never_matched_as_renames() {
        // Google native Docs have no hash. Refusing to match keeps a real delete from
        // being disguised as a move.
        let base = listing(&[("old", nohash(0, "t1"))]);
        let cur = listing(&[("new", nohash(0, "t1"))]);
        let mut d = SideDelta::compute(&base, &cur);
        d.extract_renames();
        assert!(d.renames.is_empty());
        assert_eq!(d.true_deletes(), 1);
    }

    #[test]
    fn concurrent_edits_conflict() {
        let base = listing(&[("doc.pdf", e(10, "orig"))]);
        let local = listing(&[("doc.pdf", e(11, "mine"))]);
        let remote = listing(&[("doc.pdf", e(12, "theirs"))]);
        let plan = Plan::compute("f", &base, &local, &remote);
        assert_eq!(plan.conflicts.len(), 1);
        assert_eq!(plan.conflicts[0].kind, ConflictKind::BothEdited);
        assert!(matches!(
            plan.evaluate(10, Direction::Both),
            Err(Error::Conflicts(1))
        ));
    }

    #[test]
    fn identical_concurrent_edits_are_not_a_conflict() {
        // Both sides converged on the same bytes; there is nothing to resolve.
        let base = listing(&[("doc.pdf", e(10, "orig"))]);
        let same = listing(&[("doc.pdf", e(11, "converged"))]);
        let plan = Plan::compute("f", &base, &same, &same);
        assert!(plan.conflicts.is_empty());
    }

    #[test]
    fn edit_versus_delete_conflicts_both_ways() {
        let base = listing(&[("doc.pdf", e(10, "orig"))]);
        let edited = listing(&[("doc.pdf", e(11, "new"))]);
        let gone = Listing::new();

        let p = Plan::compute("f", &base, &gone, &edited);
        assert_eq!(
            p.conflicts[0].kind,
            ConflictKind::EditedAndDeleted {
                deleted_on_local: true
            }
        );

        let p = Plan::compute("f", &base, &edited, &gone);
        assert_eq!(
            p.conflicts[0].kind,
            ConflictKind::EditedAndDeleted {
                deleted_on_local: false
            }
        );
    }

    #[test]
    fn both_sides_deleting_is_agreement_not_conflict() {
        let base = listing(&[("doc.pdf", e(10, "orig"))]);
        let gone = Listing::new();
        let plan = Plan::compute("f", &base, &gone, &gone);
        assert!(plan.conflicts.is_empty());
        assert_eq!(plan.local.true_deletes(), 1);
    }

    #[test]
    fn indeterminate_content_fails_safe() {
        // Same path created on both sides, same size, no hashes: we cannot prove they
        // match, so it must be reported rather than silently merged.
        let base = Listing::new();
        let local = listing(&[("x", nohash(5, "t1"))]);
        let remote = listing(&[("x", nohash(5, "t2"))]);
        let plan = Plan::compute("f", &base, &local, &remote);
        assert_eq!(plan.conflicts[0].kind, ConflictKind::Indeterminate);
    }

    #[test]
    fn push_asserts_no_incoming_changes() {
        let base = listing(&[("a", e(1, "h1"))]);
        let local = listing(&[("a", e(1, "h1")), ("new.pdf", e(2, "h2"))]);
        let remote_changed = listing(&[("a", e(1, "h1")), ("theirs.pdf", e(3, "h3"))]);

        // Remote untouched: push is fine.
        let p = Plan::compute("f", &base, &local, &base);
        assert!(p.evaluate(10, Direction::Push).is_ok());

        // Remote has changes: push must refuse and point at `sync`.
        let p = Plan::compute("f", &base, &local, &remote_changed);
        let err = p.evaluate(10, Direction::Push).unwrap_err();
        assert!(matches!(err, Error::Assertion(_)));
        assert!(err.to_string().contains("lode sync f"), "{err}");
    }

    #[test]
    fn pull_asserts_no_outgoing_changes() {
        let base = listing(&[("a", e(1, "h1"))]);
        let local_changed = listing(&[("a", e(1, "h1")), ("mine.pdf", e(2, "h2"))]);
        let remote = listing(&[("a", e(1, "h1")), ("theirs.pdf", e(3, "h3"))]);

        let p = Plan::compute("f", &base, &base, &remote);
        assert!(p.evaluate(10, Direction::Pull).is_ok());

        let p = Plan::compute("f", &base, &local_changed, &remote);
        assert!(matches!(
            p.evaluate(10, Direction::Pull),
            Err(Error::Assertion(_))
        ));
    }

    #[test]
    fn comparison_classifies_each_side() {
        let local = listing(&[
            ("both-same.pdf", e(1, "h1")),
            ("both-diff.pdf", e(2, "mine")),
            ("mine.pdf", e(3, "h3")),
        ]);
        let remote = listing(&[
            ("both-same.pdf", e(1, "h1")),
            ("both-diff.pdf", e(2, "theirs")),
            ("theirs.pdf", e(4, "h4")),
        ]);
        let c = Comparison::compute(&local, &remote);
        assert_eq!(c.local_only, vec!["mine.pdf".to_string()]);
        assert_eq!(c.remote_only, vec!["theirs.pdf".to_string()]);
        assert_eq!(c.differing, vec!["both-diff.pdf".to_string()]);
        assert_eq!(c.identical, 1);
        assert!(!c.in_sync());
    }

    #[test]
    fn identical_sides_compare_as_in_sync() {
        let l = listing(&[("a.pdf", e(1, "h1")), ("b.pdf", e(2, "h2"))]);
        let c = Comparison::compute(&l, &l);
        assert!(c.in_sync());
        assert_eq!(c.identical, 2);
    }

    #[test]
    fn uncomparable_content_counts_as_differing() {
        // No hash on either side: assert they match without evidence and a resync would
        // silently overwrite one with the other.
        let l = listing(&[("x", nohash(5, "t1"))]);
        let r = listing(&[("x", nohash(5, "t2"))]);
        let c = Comparison::compute(&l, &r);
        assert_eq!(c.differing, vec!["x".to_string()]);
        assert_eq!(c.identical, 0);
    }

    #[test]
    fn a_comparison_needs_no_snapshot() {
        // The whole point: this is computable when the merge base is gone.
        let empty = Listing::new();
        let remote = listing(&[("a.pdf", e(1, "h1"))]);
        let c = Comparison::compute(&empty, &remote);
        assert_eq!(c.remote_only, vec!["a.pdf".to_string()]);
        assert!(c.local_only.is_empty());
    }

    #[test]
    fn clean_plan_summarises_as_up_to_date() {
        let base = listing(&[("a", e(1, "h1"))]);
        let plan = Plan::compute("f", &base, &base, &base);
        assert!(plan.is_clean());
        assert_eq!(plan.summary(), "up to date");
        assert!(plan.evaluate(10, Direction::Both).is_ok());
        assert!(plan.evaluate(10, Direction::Push).is_ok());
        assert!(plan.evaluate(10, Direction::Pull).is_ok());
    }

    #[test]
    fn hashless_unchanged_file_uses_size_and_modtime() {
        let base = listing(&[("x", nohash(5, "t1"))]);
        let same = listing(&[("x", nohash(5, "t1"))]);
        let touched = listing(&[("x", nohash(5, "t2"))]);
        assert!(SideDelta::compute(&base, &same).modified.is_empty());
        assert_eq!(SideDelta::compute(&base, &touched).modified.len(), 1);
    }
}
