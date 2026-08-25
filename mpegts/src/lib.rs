//! MPEG-TS parsing and buffering.
//!
//! A prepared transport-stream byte buffer and the probe that reads structure
//! out of one. The two are mutually dependent - the buffer asks the probe for a
//! track signature, and the probe reports timestamp profiles the buffer defines -
//! so they form one cohesive unit rather than two layers.
//!
//! This lives below both `model` and `api` because both need it: configured
//! custom-stream responses hold prepared buffers, and the streaming and HLS
//! layers parse live ones. Nothing here may name `api`, `AppState` or a
//! repository.

pub(crate) mod transport_stream_buffer;
pub(crate) mod ts_inspector;
