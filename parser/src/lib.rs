//! Provider wire-format handling that depends on nothing above it.
//!
//! Pure transformations from a provider's wire format into playlist items and
//! EPG entries. They sit below the repository because the repository parses
//! while it persists: leaving them in `processing` made the storage layer reach
//! up into the pipeline.
//!
//! The parsers that are not here - m3u, stalker, xmltv, hls - still name the
//! pipeline, `iptv` or the API layer, and stay in `processing::parser` until
//! those references are resolved.

pub mod ics;
pub mod m3u;
pub mod m3u_format;
pub mod xtream;
