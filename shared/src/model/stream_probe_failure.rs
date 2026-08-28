//! A stream probe that did not come back with metadata.
//!
//! `ffprobe` outcomes were written to the item store and logged at `warn`,
//! and that was the whole audience: an operator learned that a provider had
//! gone dark by noticing it themselves. This is the event that closes that
//! gap, and it is the one the plugin plan wants for "alert me when a stream
//! dies" — see `plugin-system-plan.md` §7.
//!
//! # Why there is no success variant
//!
//! A metadata run probes every stream it does not already know about, so a
//! success event would fire thousands of times per refresh and carry news
//! nobody asked for. The failure is the occurrence; the success is the
//! expected case.

use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Why a probe produced no metadata.
///
/// Mirrors the actionable half of
/// `tuliprox_core::utils::ffmpeg::ProbeFailureKind`. Its `Cancelled` variant
/// has no counterpart here on purpose: a cancelled probe is a shutdown or a
/// preemption, not a statement about the stream, and reporting it as a
/// failure would teach subscribers to act on server lifecycle noise.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamProbeFailureReason {
    /// The provider answered, with a 404.
    NotFound,
    /// The probe failed or timed out.
    Unreachable,
}

/// One stream failed to probe.
///
/// # Redaction
///
/// `url` must be passed through
/// [`sanitize_sensitive_info`](crate::utils::sanitize_sensitive_info) by the
/// emitter: a provider stream URL carries the account username and password
/// in its path or query, and this record is rendered into notification
/// channels. The field is documented rather than typed as pre-sanitized
/// because the sanitizer is configuration-dependent, and a newtype implying
/// a guarantee the emitter may not have honoured would be worse than a
/// contract stated once.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StreamProbeFailure {
    /// The input whose stream this is.
    pub input: Arc<str>,
    /// The stream's id within that input, for correlating with the store.
    pub unique_id: Arc<str>,
    /// The probed URL, already sanitized. See the note above.
    pub url: Arc<str>,
    pub reason: StreamProbeFailureReason,
}

impl StreamProbeFailure {
    #[must_use]
    pub fn new(input: Arc<str>, unique_id: Arc<str>, url: Arc<str>, reason: StreamProbeFailureReason) -> Self {
        Self { input, unique_id, url, reason }
    }

    /// Deliberately per *input*, not per stream.
    ///
    /// When a provider goes down, every stream behind it fails in the same
    /// run. An operator wants to hear "this input is failing" once; a
    /// per-stream key would deliver one notification per channel and bury
    /// the signal. Plugins still see every event on the bus — this only
    /// bounds what reaches a phone.
    #[must_use]
    pub fn dedup_key(&self) -> String { format!("stream-probe:{}", self.input) }
}
