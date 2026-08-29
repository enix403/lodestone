//! Tests that mutate process-global environment variables (`HOME`, `XDG_*`) must not
//! run concurrently with each other. Cargo runs unit tests in parallel threads within a
//! single process, so they share the environment.

use std::sync::{Mutex, MutexGuard, OnceLock};

static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

pub fn env_lock() -> MutexGuard<'static, ()> {
    // Poisoning is irrelevant here: the guard protects the environment, not data
    // invariants, so a panicking test should not disable every other test.
    match ENV_LOCK.get_or_init(|| Mutex::new(())).lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    }
}
