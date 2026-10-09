use bytes::Bytes;
use futures::task::AtomicWaker;
use std::{collections::HashMap, sync::Arc};

#[derive(Clone, Debug)]
pub struct HlsPendingPesHeader {
    pub pid: u16,
    pub bytes: [u8; HLS_TS_TIMESTAMP_HEADER_BYTES],
    pub byte_offsets: [usize; HLS_TS_TIMESTAMP_HEADER_BYTES],
    pub len: usize,
    pub expected_len: Option<usize>,
    last_payload_continuity_counter: u8,
}

#[derive(Clone, Copy)]
struct HlsCompletedTimestampField {
    location: HlsTsTimestampFieldLocation,
    bytes: [u8; 5],
}

#[derive(Clone, Copy, Default)]
struct HlsCompletedPesTimestamps {
    fields: [Option<HlsCompletedTimestampField>; 2],
}

#[derive(Clone, Copy)]
struct HlsTsPacketEvidence {
    pid: u16,
    payload_unit_start: bool,
    payload_offset: Option<usize>,
    continuity_counter: u8,
    discontinuity: bool,
    pcr_field: Option<HlsTsPcrFieldLocation>,
}

#[derive(Debug)]
struct HlsPesHeaderAssembler {
    pending: HashMap<u16, HlsPendingPesHeader>,
}

#[derive(Debug)]
struct HlsTsTimestampProfileAccumulator {
    reference_clock_90khz: Option<u64>,
    earliest_relative_ticks: i64,
    latest_relative_ticks: i64,
    maximum_span_ticks_90khz: u64,
    observed_pts_or_dts: bool,
    observed_pcr: bool,
    observation_count: u64,
    invalid: bool,
}

#[derive(Debug)]
pub struct HlsTsTimestampProfileScanner {
    assembler: HlsPesHeaderAssembler,
    accumulator: HlsTsTimestampProfileAccumulator,
    next_packet_start: usize,
    invalid: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct HlsFiniteTsPacketLayout {
    packet_start: usize,
    pid: u16,
    has_payload: bool,
    timestamp_field_indices_start: usize,
    timestamp_field_indices_end: usize,
    pcr_field_index: Option<usize>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HlsFiniteTsLayout {
    packets: Arc<[HlsFiniteTsPacketLayout]>,
    pub timestamp_fields: Arc<[HlsTsTimestampFieldLocation]>,
    pub pcr_fields: Arc<[HlsTsPcrFieldLocation]>,
    packet_timestamp_field_indices: Arc<[usize]>,
}

pub struct TransportStreamBuffer {
    // `Bytes` instead of `Arc<Vec<u8>>`: cloning the inner payload (which
    // happens once per HLS-CVS fallback response) is then a refcount bump
    // instead of a deep copy. All existing `&self.buffer[..]` / `.len()` /
    // `.is_empty()` call sites continue to work via `Bytes`'s `Deref<Target=[u8]>`.
    buffer: Bytes,
    packet_starts: Arc<[usize]>,
    finite_hls_layout: Result<Arc<HlsFiniteTsLayout>, HlsFiniteTsLayoutError>,
    finite_hls_presentation_duration: Option<HlsTsPresentationDuration>,
    current_pos: usize,
    current_dts: u64,
    timestamp_offset: u64,
    length: usize,
    /// Per-PID continuity counter and discontinuity-sent flag.
    /// Indexed directly by PID (0–8191) for O(1) lookup.
    cc_entries: Box<[Option<(u8, bool)>; 8192]>,
    waker: Arc<AtomicWaker>,
    first_pcr: Option<u64>,
    finite_hls_timestamp_profile: Option<HlsTsTimestampProfile>,
    finite_hls_track_signature: Option<crate::ts_inspector::HlsTsTrackSignature>,
    finite_hls_asset_fingerprint: [u8; 32],
    #[cfg(any(test, feature = "test-support"))]
    finite_hls_render_count: Arc<std::sync::atomic::AtomicUsize>,
    #[cfg(any(test, feature = "test-support"))]
    finite_hls_finalize_count: Arc<std::sync::atomic::AtomicUsize>,
    force_discontinuity_on_wrap: bool,
}

#[cfg(test)]
mod tests;

mod buffer;
mod clock;
mod layout;
mod pes;
mod profile;
mod render;
use clock::HLS_TS_TIMESTAMP_HEADER_BYTES;
#[cfg(test)]
use clock::{MAX_PCR, PACKET_COUNT};
pub use layout::{HlsTsPidPresentationTimeline, HlsTsPresentationClockSource, HlsTsPresentationDuration};
use pes::HlsFiniteTsLayoutError;
pub use pes::{HlsTsPcrFieldLocation, HlsTsTimestampFieldKind, HlsTsTimestampFieldLocation};
pub use profile::{HlsTsSpliceAnchor, HlsTsTimestampProfile};
#[allow(unused_imports, reason = "Retains the existing module interface in production and test builds.")]
pub use render::{
    HlsFiniteTsDiscontinuityMode, HlsFiniteTsFinalizeSpec, HlsFiniteTsRenderError, HlsFiniteTsRenderSpec,
};
