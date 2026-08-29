//! Cross-platform filename hazards.
//!
//! These are the failure modes that a macOS + Linux fleet produces silently, where the
//! symptom is not an error but files that duplicate, ping-pong, or quietly vanish. Each is
//! cheap to detect from listings lodestone already has, and each aborts rather than warns:
//! there is no safe automatic resolution for any of them.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use unicode_normalization::UnicodeNormalization;

/// Why a group of distinct paths may be conflated by some filesystem.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CollisionKind {
    /// Same name once Unicode-normalised: macOS (APFS/HFS+) stores filenames decomposed —
    /// NFD — so `Résumé.pdf` created there is `Re` + combining acute + `sume´.pdf`, while
    /// Linux stores what it was given, in practice NFC. Different bytes, one visible name,
    /// so these become phantom duplicates that ping-pong between machines forever.
    Normalisation,
    /// Same name once case is folded too: APFS is case-insensitive by default, ext4 is not.
    /// If a Linux machine holds `Report.pdf` and `report.pdf` at once, a Mac physically
    /// cannot represent both and the sync either clobbers one or loops.
    Case,
}

impl CollisionKind {
    pub fn label(self) -> &'static str {
        match self {
            CollisionKind::Normalisation => "unicode normalisation",
            CollisionKind::Case => "case",
        }
    }
}

/// Two or more distinct paths that a filesystem may be unable to tell apart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Collision {
    pub kind: CollisionKind,
    /// The distinct paths that collide, sorted.
    pub paths: Vec<String>,
}

/// Every group of paths some filesystem in a mixed fleet could conflate.
///
/// One pass, not two. Grouping by "normalised and case-folded" is the broadest predicate —
/// case-insensitive filesystems are also normalisation-insensitive in practice — so a
/// second, narrower pass over normalisation alone would only ever re-report the same pairs.
/// Instead each group is labelled by *why* it collides: if every path in it shares one NFC
/// form, case is not involved and it is a pure normalisation collision.
pub fn name_collisions(paths: &[String]) -> Vec<Collision> {
    let mut groups: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for p in paths {
        let key = p.nfc().collect::<String>().to_lowercase();
        groups.entry(key).or_default().push(p.clone());
    }
    groups
        .into_values()
        .filter_map(|mut paths| {
            paths.sort();
            paths.dedup();
            if paths.len() < 2 {
                return None;
            }
            let nfc = |p: &String| p.nfc().collect::<String>();
            let first = nfc(&paths[0]);
            let kind = if paths.iter().all(|p| nfc(p) == first) {
                CollisionKind::Normalisation
            } else {
                CollisionKind::Case
            };
            Some(Collision { kind, paths })
        })
        .collect()
}

/// Symlinks in the local tree.
///
/// rclone skips them by default, so they are simply absent from the remote. Silently
/// missing files are the worst kind of surprise, so they are reported — but not treated as
/// an error, since skipping is the intended behaviour.
pub fn find_symlinks(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out.sort();
    out
}

fn walk(root: &Path, dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in rd.flatten() {
        let path = entry.path();
        // `symlink_metadata` does not follow the link, which is the whole point.
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.file_type().is_symlink() {
            if let Ok(rel) = path.strip_prefix(root) {
                out.push(rel.to_path_buf());
            }
        } else if meta.is_dir() {
            walk(root, &path, out);
        }
    }
}

/// Probe whether a directory lives on a case-insensitive filesystem.
///
/// Answered by experiment rather than by guessing from the platform: APFS is
/// case-insensitive *by default* but can be formatted either way, and a Linux box may well
/// have a folder on an exFAT or NTFS volume.
///
/// Returns `None` if the probe could not be performed.
pub fn is_case_insensitive(dir: &Path) -> Option<bool> {
    let lower = dir.join(".lode-case-probe");
    let upper = dir.join(".LODE-CASE-PROBE");
    std::fs::write(&lower, b"").ok()?;
    let answer = upper.exists();
    let _ = std::fs::remove_file(&lower);
    Some(answer)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn detects_nfc_versus_nfd_pairs() {
        // The same visible name, decomposed (as macOS stores it) and composed (as Linux
        // usually does).
        let nfd = "inbox/Re\u{301}sume\u{301}.pdf";
        let nfc = "inbox/Résumé.pdf";
        assert_ne!(nfd, nfc, "these must be different byte strings");

        let found = name_collisions(&v(&[nfd, nfc, "inbox/other.pdf"]));
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].kind, CollisionKind::Normalisation);
        assert_eq!(found[0].paths.len(), 2);
        assert!(found[0].paths.contains(&nfd.to_string()));
        assert!(found[0].paths.contains(&nfc.to_string()));
    }

    #[test]
    fn plain_ascii_names_never_collide_on_normalisation() {
        assert!(name_collisions(&v(&["a.pdf", "b.pdf", "deep/c.pdf"])).is_empty());
    }

    #[test]
    fn identical_paths_are_not_a_collision() {
        // A listing cannot hold the same path twice, but the grouping must not invent a
        // collision if it ever did.
        assert!(name_collisions(&v(&["a.pdf", "a.pdf"])).is_empty());
    }

    #[test]
    fn detects_case_only_collisions() {
        let found = name_collisions(&v(&["inbox/Report.pdf", "inbox/report.pdf", "x.pdf"]));
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].kind, CollisionKind::Case);
        assert_eq!(
            found[0].paths,
            vec![
                "inbox/Report.pdf".to_string(),
                "inbox/report.pdf".to_string()
            ]
        );
    }

    #[test]
    fn case_check_is_not_fooled_by_directory_case() {
        // Differing only in the case of a parent directory is just as much a problem.
        let found = name_collisions(&v(&["Inbox/a.pdf", "inbox/a.pdf"]));
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].kind, CollisionKind::Case);
    }

    #[test]
    fn a_pair_differing_in_both_case_and_normalisation_is_reported_once_as_case() {
        let nfd_upper = "Re\u{301}sume\u{301}.pdf";
        let nfc_lower = "résumé.pdf";
        let found = name_collisions(&v(&[nfd_upper, nfc_lower]));
        assert_eq!(found.len(), 1, "should be a single collision, not two");
        assert_eq!(
            found[0].kind,
            CollisionKind::Case,
            "case is the broader cause"
        );
        assert_eq!(found[0].paths.len(), 2);
    }

    #[test]
    fn unrelated_names_are_never_grouped() {
        let found = name_collisions(&v(&["a.pdf", "A.pdf", "b.pdf", "c.pdf"]));
        assert_eq!(found.len(), 1);
        assert_eq!(
            found[0].paths,
            vec!["A.pdf".to_string(), "a.pdf".to_string()]
        );
    }

    #[test]
    fn finds_symlinks_without_following_them() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("inbox")).unwrap();
        std::fs::write(root.join("inbox/real.pdf"), b"x").unwrap();
        std::os::unix::fs::symlink(root.join("inbox/real.pdf"), root.join("inbox/link.pdf"))
            .unwrap();
        // A symlink pointing nowhere must still be found, not skipped as unreadable.
        std::os::unix::fs::symlink(root.join("missing"), root.join("dangling")).unwrap();

        let found = find_symlinks(root);
        assert_eq!(found.len(), 2, "{found:?}");
        assert!(found.contains(&PathBuf::from("inbox/link.pdf")));
        assert!(found.contains(&PathBuf::from("dangling")));
    }

    #[test]
    fn no_symlinks_in_an_ordinary_tree() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("a/b")).unwrap();
        std::fs::write(tmp.path().join("a/b/c.pdf"), b"x").unwrap();
        assert!(find_symlinks(tmp.path()).is_empty());
    }

    #[test]
    fn the_case_probe_answers_and_cleans_up() {
        let tmp = tempfile::tempdir().unwrap();
        let answer = is_case_insensitive(tmp.path());
        assert!(answer.is_some(), "probe should succeed on a writable dir");
        // Whatever the answer, no probe file may be left behind.
        let leftovers: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }
}
