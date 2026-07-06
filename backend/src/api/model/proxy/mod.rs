//! Cross-proxy shared helpers (header policy, request building, etc.).
//!
//! Lives next to the existing `api::model::*` modules and is intentionally small.
//! `header_policy` is the canonical home for hop-by-hop header rules used by both
//! the HLS cache proxy and the MPEG-TS reverse proxy — see its module docs.

pub mod header_policy;
