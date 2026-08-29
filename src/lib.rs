//! lodestone — manual, git-like bidirectional folder sync on top of `rclone bisync`.
//!
//! See `docs/TDD.md` for the full design. The short version:
//!
//! * Every mutating command runs in two phases: **plan** then **apply**.
//! * The plan is computed by lodestone itself as a three-way delta between a stored
//!   snapshot (the "merge base"), the current local listing, and the current remote
//!   listing. rclone is the *executor*, not the planner.
//! * Renames are detected by matching unmatched deletes against unmatched creates on
//!   content hash, symmetrically on both sides. Only deletes whose content vanishes
//!   entirely count against the delete guard.

pub mod config;
pub mod error;
pub mod machine;
pub mod paths;
pub mod plan;
pub mod rclone;
pub mod snapshot;
#[cfg(test)]
pub(crate) mod testlock;

pub use error::{Error, ExitCode, Result};
