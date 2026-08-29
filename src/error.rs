use std::fmt;

/// Process exit codes. These are part of lodestone's public contract: wrapper scripts
/// are expected to branch on them (retry a 13, never retry a 10).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum ExitCode {
    Ok = 0,
    Unexpected = 1,
    Usage = 2,
    Conflict = 10,
    DeleteGuard = 11,
    Assertion = 12,
    Unreachable = 13,
}

impl ExitCode {
    pub fn as_i32(self) -> i32 {
        self as i32
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("config error: {0}")]
    Config(String),

    #[error("rclone not found on PATH. Install rclone: https://rclone.org/install/")]
    RcloneMissing,

    #[error(
        "rclone {found} is below the required minimum {required}.\n\
         Distro packages are frequently years stale. Install a current rclone with:\n\
         \x20 curl https://rclone.org/install.sh | sudo bash"
    )]
    RcloneTooOld { found: String, required: String },

    #[error("could not parse rclone version from: {0:?}")]
    RcloneVersionUnparseable(String),

    #[error("rclone failed (exit {code}): {stderr}")]
    RcloneFailed { code: i32, stderr: String },

    #[error("remote {0:?} is not defined in rclone.conf (see `rclone listremotes`)")]
    UnknownRemote(String),

    #[error("folder {0:?} is not configured")]
    UnknownFolder(String),

    #[error("folder {0:?} has no snapshot yet — run `lode init {0}` first")]
    NotInitialised(String),

    #[error(
        "snapshot for folder {folder:?} was written by machine {stored:?} but this is {current:?}.\n\
         Snapshots are machine-local and must never be synced between machines."
    )]
    ForeignSnapshot {
        folder: String,
        stored: String,
        current: String,
    },

    #[error(
        "lodestone state directory {state:?} is inside synced folder {folder:?}.\n\
         State must never be synced. Move it or unset XDG_STATE_HOME."
    )]
    StateInsideSyncedFolder { state: String, folder: String },

    #[error("{0} conflict(s) require manual resolution")]
    Conflicts(usize),

    #[error(
        "delete guard tripped on the {side} side: {found} true delete(s) exceeds the limit of {limit}.\n\
         These are files whose content does not reappear anywhere else, so they are not moves.\n\
         Review them with `lode status`, then re-run with `--allow-deletes {found}` if intended."
    )]
    DeleteGuard {
        side: String,
        found: usize,
        limit: usize,
    },

    #[error("{0}")]
    Assertion(String),

    #[error(
        "folder {0:?} has a stale bisync lock: a previous run was interrupted.\n\
         Make sure no other `lode` is running, then clear it with `lode unlock {0}`."
    )]
    StaleLock(String),

    #[error(
        "bisync refuses to run for folder {0:?} without a new baseline.\n\
         Review `lode status {0}`, then re-baseline deliberately with:\n\
         \x20 lode resync {0} --i-understand"
    )]
    NeedsResync(String),

    #[error(
        "syncing folder {0:?} would leave one side with no files at all, and rclone refuses \
         to sync to an empty directory.\n\
         This is rclone's own floor and is independent of --allow-deletes.\n\
         If emptying it is genuinely what you want, clear the other side directly, then:\n\
         \x20 lode resync {0} --i-understand"
    )]
    EmptySide(String),

    #[error("io error at {path:?}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },

    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

impl Error {
    /// Map an error onto the exit code contract documented in the TDD.
    pub fn exit_code(&self) -> ExitCode {
        match self {
            Error::Config(_) | Error::UnknownFolder(_) | Error::UnknownRemote(_) => ExitCode::Usage,
            Error::Conflicts(_) => ExitCode::Conflict,
            Error::DeleteGuard { .. } => ExitCode::DeleteGuard,
            Error::Assertion(_) => ExitCode::Assertion,
            Error::RcloneFailed { .. } => ExitCode::Unreachable,
            _ => ExitCode::Unexpected,
        }
    }

    pub fn io(path: impl fmt::Display, source: std::io::Error) -> Self {
        Error::Io {
            path: path.to_string(),
            source,
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;
