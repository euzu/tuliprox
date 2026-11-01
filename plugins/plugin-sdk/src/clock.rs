//! The host clock. Hooks must be a pure function of their inputs and the
//! plugin's KV (plan §7 *Determinism*): the wall clock is not an input, so
//! there is deliberately no `std::time::SystemTime` shim. Read time here, so a
//! frozen host clock freezes the plugin too.

use crate::sys;

/// Seconds since the unix epoch, from the host clock.
#[must_use]
pub fn now_unix() -> i64 { sys::now_unix() }
