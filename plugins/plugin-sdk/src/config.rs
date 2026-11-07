//! The raw settings JSON. Prefer a `#[tuliprox_plugin_settings]` struct.

use crate::sys;

/// The settings JSON of this instance, secrets resolved.
#[must_use]
pub fn raw() -> String { sys::get_config() }
