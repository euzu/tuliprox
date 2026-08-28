//! Duration newtypes for config fields whose unit lived only in their name.
//!
//! Across the workspace roughly 245 config and state fields encode their unit as
//! a naming convention — `origin_manifest_timeout_ms`, `initial_manifest_wait_
//! timeout_secs`, `throttle_kbps` — and several sit adjacent inside the same
//! struct. Nothing stopped a millisecond value reaching a seconds parameter; the
//! compiler saw `u64` either way, and `session_idle_timeout` did not even carry
//! the unit in its name (it is seconds).
//!
//! [`Millis`] and [`Secs`] make the unit part of the type. Both are
//! `#[repr(transparent)]` over the `u64` they replace and `#[serde(transparent)]`
//! in their serialized form, so:
//!
//! * the in-memory layout is unchanged — this is not a size or indirection cost;
//! * the on-disk YAML/JSON shape is byte-identical, so the change lands with no
//!   config migration and nothing user-visible.
//!
//! The conversions are methods rather than `From` impls on purpose: `Secs ->
//! Millis` is a multiplication that can overflow, and making it explicit at the
//! call site is the point of having the types at all.

use serde::{Deserialize, Serialize};
use std::{fmt::Display, time::Duration};

/// A duration in whole milliseconds.
#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize)]
#[repr(transparent)]
#[serde(transparent)]
pub struct Millis(pub u64);

impl Millis {
    #[inline]
    pub const fn new(value: u64) -> Self { Self(value) }

    /// The raw count. Prefer [`Self::as_duration`] where a `Duration` will do.
    #[inline]
    pub const fn get(self) -> u64 { self.0 }

    #[inline]
    pub const fn as_duration(self) -> Duration { Duration::from_millis(self.0) }

    /// As a `Duration`, floored at one millisecond.
    ///
    /// Several origin deadlines treat a zero timeout as "do not wait at all"
    /// rather than "wait forever", so they clamped with `.max(1)` at each call
    /// site. This keeps that behaviour in one place.
    #[inline]
    pub const fn as_duration_at_least_1ms(self) -> Duration {
        Duration::from_millis(if self.0 == 0 { 1 } else { self.0 })
    }
}

impl Display for Millis {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { write!(f, "{}ms", self.0) }
}

/// A duration in whole seconds.
#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize)]
#[repr(transparent)]
#[serde(transparent)]
pub struct Secs(pub u64);

impl Secs {
    #[inline]
    pub const fn new(value: u64) -> Self { Self(value) }

    /// The raw count. Prefer [`Self::as_duration`] where a `Duration` will do.
    #[inline]
    pub const fn get(self) -> u64 { self.0 }

    #[inline]
    pub const fn as_duration(self) -> Duration { Duration::from_secs(self.0) }

    /// Convert to milliseconds, saturating rather than wrapping.
    #[inline]
    pub const fn as_millis(self) -> Millis { Millis(self.0.saturating_mul(1_000)) }
}

impl Display for Secs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { write!(f, "{}s", self.0) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serialized_form_is_a_bare_number_so_configs_need_no_migration() {
        assert_eq!(serde_json::to_string(&Millis(3_000)).expect("Millis serializes"), "3000");
        assert_eq!(serde_json::to_string(&Secs(90)).expect("Secs serializes"), "90");
        assert_eq!(serde_json::from_str::<Millis>("3000").expect("Millis parses"), Millis(3_000));
        assert_eq!(serde_json::from_str::<Secs>("90").expect("Secs parses"), Secs(90));
    }

    #[test]
    fn layout_is_transparent_over_u64() {
        assert_eq!(size_of::<Millis>(), size_of::<u64>());
        assert_eq!(size_of::<Secs>(), size_of::<u64>());
        assert_eq!(size_of::<Option<Millis>>(), size_of::<Option<u64>>());
    }

    #[test]
    fn secs_to_millis_saturates_instead_of_wrapping() {
        assert_eq!(Secs(2).as_millis(), Millis(2_000));
        assert_eq!(Secs(u64::MAX).as_millis(), Millis(u64::MAX));
    }

    #[test]
    fn zero_millisecond_timeout_floors_to_one() {
        assert_eq!(Millis(0).as_duration_at_least_1ms(), Duration::from_millis(1));
        assert_eq!(Millis(5).as_duration_at_least_1ms(), Duration::from_millis(5));
        assert_eq!(Millis(0).as_duration(), Duration::ZERO);
    }
}
