//! A per-folder advisory lock, held for the duration of a mutating run.
//!
//! bisync has its own lock, so two concurrent runs were never going to corrupt anything —
//! the second one aborted. But it aborted with `prior lock file found`, which lodestone
//! reports as a *stale* lock and tells you to clear with `lode unlock`. That advice is
//! actively wrong when the other run is alive and simply still working, and following it
//! would remove a lock that is doing its job.
//!
//! This lock exists to give that case an honest message, before rclone is ever invoked.
//!
//! Read-only commands (`status`, `compare`, `log`) do not take it: several of those running
//! at once are harmless.

use crate::error::{Error, Result};
use crate::{machine, paths, timestamp};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Holder {
    pub pid: u32,
    pub host: String,
    pub at: u64,
}

/// Released on drop, so an early return or a panic cannot leave it behind.
#[derive(Debug)]
pub struct Lock {
    path: PathBuf,
    /// False when the lock was not actually taken (see [`Lock::none`]).
    held: bool,
}

impl Lock {
    pub fn path_for(folder: &str) -> PathBuf {
        paths::folder_state_dir(folder).join("lock")
    }

    /// A no-op guard, for code paths that deliberately do not lock.
    pub fn none() -> Self {
        Lock {
            path: PathBuf::new(),
            held: false,
        }
    }

    pub fn acquire(folder: &str) -> Result<Self> {
        let path = Self::path_for(folder);
        let dir = paths::folder_state_dir(folder);
        std::fs::create_dir_all(&dir).map_err(|e| Error::io(dir.display(), e))?;

        match Self::try_create(&path) {
            Ok(()) => Ok(Lock { path, held: true }),
            Err(existing) => {
                // Someone holds it. Alive means wait; dead means it was interrupted, and
                // reclaiming is safe because the pid is gone.
                match existing {
                    Some(h) if is_alive(&h) => Err(Error::LockHeld {
                        folder: folder.to_string(),
                        pid: h.pid,
                        since: timestamp::format_rfc3339(h.at),
                    }),
                    _ => {
                        let _ = std::fs::remove_file(&path);
                        Self::try_create(&path).map_err(|_| Error::LockHeld {
                            folder: folder.to_string(),
                            pid: 0,
                            since: "unknown".into(),
                        })?;
                        Ok(Lock { path, held: true })
                    }
                }
            }
        }
    }

    /// Create the lock file exclusively. `Err(Some(holder))` when it already exists and
    /// could be read, `Err(None)` when it exists but is unreadable or malformed.
    fn try_create(path: &PathBuf) -> std::result::Result<(), Option<Holder>> {
        match std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(path)
        {
            Ok(mut f) => {
                let holder = Holder {
                    pid: std::process::id(),
                    host: machine::hostname(),
                    at: timestamp::now_unix(),
                };
                // A failed write leaves an empty file, which reads back as unparseable and
                // is therefore treated as reclaimable rather than blocking forever.
                let _ = writeln!(f, "{}", serde_json::to_string(&holder).unwrap_or_default());
                Ok(())
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Err(read_holder(path)),
            // Anything else (permissions, read-only fs) is not a held lock.
            Err(_) => Err(None),
        }
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        if self.held {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

fn read_holder(path: &PathBuf) -> Option<Holder> {
    let body = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(body.trim()).ok()
}

/// Whether the recorded holder is still running.
///
/// Only meaningful on the machine that wrote it — a pid from another host says nothing
/// about a process here — so a foreign host is treated as alive rather than reclaimed.
fn is_alive(h: &Holder) -> bool {
    if h.host != machine::hostname() {
        return true;
    }
    // `kill -0` is the portable liveness check and needs no libc dependency.
    std::process::Command::new("kill")
        .args(["-0", &h.pid.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Remove a folder's lock unconditionally, for `lode unlock`.
pub fn clear(folder: &str) -> Result<bool> {
    let path = Lock::path_for(folder);
    if !path.exists() {
        return Ok(false);
    }
    std::fs::remove_file(&path).map_err(|e| Error::io(path.display(), e))?;
    Ok(true)
}

/// Who holds a folder's lock, if anyone.
pub fn holder(folder: &str) -> Option<Holder> {
    read_holder(&Lock::path_for(folder))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_state<T>(f: impl FnOnce() -> T) -> T {
        let _g = crate::testlock::env_lock();
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("XDG_STATE_HOME", tmp.path());
        let out = f();
        std::env::remove_var("XDG_STATE_HOME");
        out
    }

    #[test]
    fn acquiring_records_this_process_and_releasing_removes_it() {
        with_state(|| {
            let lock = Lock::acquire("docs").unwrap();
            let h = holder("docs").expect("lock file should be readable");
            assert_eq!(h.pid, std::process::id());
            assert_eq!(h.host, machine::hostname());
            drop(lock);
            assert!(holder("docs").is_none(), "drop must release");
        });
    }

    #[test]
    fn a_live_holder_blocks_a_second_acquisition() {
        with_state(|| {
            let _held = Lock::acquire("docs").unwrap();
            // This process is obviously alive, so a second attempt must be refused.
            let err = Lock::acquire("docs").unwrap_err();
            match &err {
                Error::LockHeld { folder, pid, .. } => {
                    assert_eq!(folder, "docs");
                    assert_eq!(*pid, std::process::id());
                }
                other => panic!("expected LockHeld, got {other:?}"),
            }
            // The message must not send the user to `unlock` for a lock that is working.
            assert!(err.to_string().contains("still running"), "{err}");
        });
    }

    #[test]
    fn a_dead_holder_is_reclaimed() {
        with_state(|| {
            std::fs::create_dir_all(paths::folder_state_dir("docs")).unwrap();
            // pid 2^31-ish will not exist; simulate a run killed mid-flight.
            let stale = Holder {
                pid: 2_000_000_000,
                host: machine::hostname(),
                at: 1,
            };
            std::fs::write(
                Lock::path_for("docs"),
                serde_json::to_string(&stale).unwrap(),
            )
            .unwrap();

            let lock = Lock::acquire("docs").expect("a dead holder must not block forever");
            assert_eq!(holder("docs").unwrap().pid, std::process::id());
            drop(lock);
        });
    }

    #[test]
    fn a_lock_from_another_host_is_never_reclaimed() {
        with_state(|| {
            std::fs::create_dir_all(paths::folder_state_dir("docs")).unwrap();
            // A pid that certainly exists here, but attributed to a different machine:
            // it says nothing about a process on this one, so it must not be reclaimed.
            let foreign = Holder {
                pid: std::process::id(),
                host: "some-other-host".into(),
                at: 1,
            };
            std::fs::write(
                Lock::path_for("docs"),
                serde_json::to_string(&foreign).unwrap(),
            )
            .unwrap();
            assert!(Lock::acquire("docs").is_err());
        });
    }

    #[test]
    fn an_unreadable_lock_is_reclaimed_rather_than_blocking_forever() {
        with_state(|| {
            std::fs::create_dir_all(paths::folder_state_dir("docs")).unwrap();
            // What a crash between create and write leaves behind.
            std::fs::write(Lock::path_for("docs"), b"").unwrap();
            let lock = Lock::acquire("docs").expect("an empty lock must not wedge the folder");
            drop(lock);
        });
    }

    #[test]
    fn clear_removes_a_lock_and_reports_whether_there_was_one() {
        with_state(|| {
            assert!(!clear("docs").unwrap());
            std::mem::forget(Lock::acquire("docs").unwrap());
            assert!(clear("docs").unwrap());
            assert!(holder("docs").is_none());
        });
    }

    #[test]
    fn the_no_op_guard_touches_nothing() {
        with_state(|| {
            let l = Lock::none();
            drop(l);
            assert!(holder("docs").is_none());
        });
    }
}
