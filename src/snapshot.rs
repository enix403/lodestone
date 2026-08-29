//! The snapshot: lodestone's "merge base".
//!
//! Git can offer directional `push`/`pull` because it knows the common ancestor. A naive
//! one-directional mirror cannot distinguish "I deleted this" from "they added this",
//! which is how sync tools destroy data. The snapshot is that common ancestor: the state
//! of the tree as of the last successful sync, recorded once for both sides.
//!
//! It is machine-local and must never be synced. See [`crate::machine`].

use crate::error::{Error, Result};
use crate::paths;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// One file, as seen by `rclone lsjson`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub size: u64,
    #[serde(default)]
    pub modtime: String,
    /// Content hash (md5). `None` when the backend cannot supply one — notably Google
    /// native Docs/Sheets, which have neither a hash nor a meaningful size.
    #[serde(default)]
    pub hash: Option<String>,
}

impl Entry {
    pub fn new(size: u64, modtime: impl Into<String>, hash: Option<String>) -> Self {
        Self {
            size,
            modtime: modtime.into(),
            hash: hash.filter(|h| !h.is_empty()),
        }
    }

    /// Whether two entries hold the same bytes.
    ///
    /// Returns `None` when this cannot be determined (either side lacks a hash), so
    /// callers must decide explicitly rather than silently treating unknown as equal.
    pub fn same_content(&self, other: &Entry) -> Option<bool> {
        if self.size != other.size {
            return Some(false);
        }
        match (&self.hash, &other.hash) {
            (Some(a), Some(b)) => Some(a == b),
            _ => None,
        }
    }
}

/// A listing: relative path -> entry. `BTreeMap` so all output is deterministically
/// ordered, which matters for diffable test assertions and readable status output.
pub type Listing = BTreeMap<String, Entry>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    /// Snapshot format version, so a future change can be detected rather than
    /// misparsed.
    pub version: u32,
    pub folder: String,
    pub machine_id: String,
    /// RFC3339 timestamp of the sync that produced this snapshot.
    pub taken_at: String,
    /// The agreed state of both sides after that sync.
    pub entries: Listing,
}

pub const SNAPSHOT_VERSION: u32 = 1;

impl Snapshot {
    pub fn new(folder: &str, machine_id: &str, entries: Listing) -> Self {
        Self {
            version: SNAPSHOT_VERSION,
            folder: folder.to_string(),
            machine_id: machine_id.to_string(),
            taken_at: now_rfc3339(),
            entries,
        }
    }

    pub fn load(folder: &str, machine_id: &str) -> Result<Self> {
        let path = paths::snapshot_path(folder);
        let raw = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(Error::NotInitialised(folder.to_string()))
            }
            Err(e) => return Err(Error::io(path.display(), e)),
        };
        let snap: Snapshot = serde_json::from_str(&raw)?;
        if snap.machine_id != machine_id {
            return Err(Error::ForeignSnapshot {
                folder: folder.to_string(),
                stored: snap.machine_id,
                current: machine_id.to_string(),
            });
        }
        Ok(snap)
    }

    pub fn exists(folder: &str) -> bool {
        paths::snapshot_path(folder).exists()
    }

    /// Write atomically: a partially-written snapshot would be a corrupt merge base, and
    /// the next run would compute a wrong plan against it.
    pub fn save(&self) -> Result<()> {
        let path = paths::snapshot_path(&self.folder);
        let dir = paths::folder_state_dir(&self.folder);
        std::fs::create_dir_all(&dir).map_err(|e| Error::io(dir.display(), e))?;
        let tmp = path.with_extension("json.tmp");
        let body = serde_json::to_string_pretty(self)?;
        std::fs::write(&tmp, body).map_err(|e| Error::io(tmp.display(), e))?;
        std::fs::rename(&tmp, &path).map_err(|e| Error::io(path.display(), e))?;
        Ok(())
    }
}

fn now_rfc3339() -> String {
    // Seconds-resolution UTC without pulling in a date library. This value is displayed,
    // never parsed for logic.
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (y, mo, d, h, mi, s) = civil_from_unix(secs as i64);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

/// Howard Hinnant's `civil_from_days`, the standard branch-free epoch->calendar
/// conversion.
fn civil_from_unix(secs: i64) -> (i64, u32, u32, u32, u32, u32) {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (
        y,
        m,
        d,
        (rem / 3600) as u32,
        ((rem % 3600) / 60) as u32,
        (rem % 60) as u32,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_content_is_tri_state() {
        let a = Entry::new(10, "t", Some("aaa".into()));
        let b = Entry::new(10, "t", Some("aaa".into()));
        let c = Entry::new(10, "t", Some("bbb".into()));
        let d = Entry::new(20, "t", Some("aaa".into()));
        let no_hash = Entry::new(10, "t", None);

        assert_eq!(a.same_content(&b), Some(true));
        assert_eq!(a.same_content(&c), Some(false));
        // Differing size short-circuits: definitely different, no hash needed.
        assert_eq!(a.same_content(&d), Some(false));
        // Same size but no hash available: genuinely unknown.
        assert_eq!(a.same_content(&no_hash), None);
    }

    #[test]
    fn empty_hash_is_normalised_to_none() {
        assert_eq!(Entry::new(1, "t", Some(String::new())).hash, None);
    }

    #[test]
    fn civil_conversion_matches_known_dates() {
        assert_eq!(civil_from_unix(0), (1970, 1, 1, 0, 0, 0));
        assert_eq!(civil_from_unix(1_000_000_000), (2001, 9, 9, 1, 46, 40));
        // A leap day.
        assert_eq!(civil_from_unix(1_709_164_800), (2024, 2, 29, 0, 0, 0));
    }
}
