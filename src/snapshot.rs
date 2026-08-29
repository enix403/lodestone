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
    /// Fingerprint of the filter set in force when this snapshot was taken. bisync
    /// demands a resync when its filters change; recording this lets lodestone explain
    /// *why* rather than passing rclone's cryptic demand through. Empty for snapshots
    /// written before filtering existed.
    #[serde(default)]
    pub filters: String,
    /// The agreed state of both sides after that sync.
    pub entries: Listing,
}

pub const SNAPSHOT_VERSION: u32 = 1;

/// The hostname portion of a machine id (`<hostname>-<random hex>`).
fn host_of(id: &str) -> &str {
    id.rsplit_once('-').map(|(host, _)| host).unwrap_or(id)
}

impl Snapshot {
    pub fn new(folder: &str, machine_id: &str, entries: Listing) -> Self {
        Self {
            version: SNAPSHOT_VERSION,
            folder: folder.to_string(),
            machine_id: machine_id.to_string(),
            taken_at: now_rfc3339(),
            filters: crate::filters::fingerprint(),
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
            // Same host, different id: the id file was regenerated rather than the
            // snapshot arriving from elsewhere. Very different cause, so say so.
            if host_of(&snap.machine_id) == host_of(machine_id) {
                return Err(Error::MachineIdChanged(
                    folder.to_string(),
                    paths::state_dir().join("machine.id").display().to_string(),
                ));
            }
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
    crate::timestamp::format_rfc3339(crate::timestamp::now_unix())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn machine_id_host_is_split_off_correctly() {
        assert_eq!(host_of("radium-fed-18d0e7aa75978294"), "radium-fed");
        assert_eq!(host_of("MT-H0Y07-Qateef-18d0"), "MT-H0Y07-Qateef");
        // No suffix at all: treat the whole thing as the host rather than panicking.
        assert_eq!(host_of("plain"), "plain");
    }

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
}
