//! Epoch-seconds view of the workspace clock.
//!
//! Stalker portals speak in whole seconds — cookie `Max-Age`, `Expires`, session age —
//! while [`tuliprox_core::utils::Clock`] reports milliseconds. This is the one conversion,
//! and the one place in the crate that reads the wall clock without being handed an
//! instant by its caller.

use tuliprox_core::utils::{Clock, SystemClock};

/// Unix-epoch seconds according to `clock`.
#[inline]
pub fn epoch_secs<C: Clock>(clock: &C) -> u64 { clock.now_ms().get() / 1_000 }

/// Unix-epoch seconds according to the system clock.
///
/// For the call sites that have no clock to hand. Prefer taking the instant as a parameter
/// — every expiry rule in this crate does — or holding a `C: Clock`.
#[inline]
#[must_use]
pub fn system_epoch_secs() -> u64 { epoch_secs(&SystemClock) }
