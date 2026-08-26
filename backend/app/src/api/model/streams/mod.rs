//! The streaming layer.
//!
//! This stays in `api` rather than joining `tuliprox-session`, and not because
//! of `AppState`. Two independent things pin it here:
//!
//! - `active_client_stream` and `provider_stream` call panel provisioning
//!   (`can_provision_on_exhausted`, `run_panel_api_provisioning_probe`,
//!   `find_input_by_provider_name`), which takes the whole root state and does
//!   HTTP against an external panel.
//! - `provider_stream_factory` logs origin content-coding through
//!   `tuliprox-hls`, and `tuliprox-hls` already depends on `tuliprox-session`,
//!   so moving it down would close a cycle.
//!
//! Together those say something real: the streaming layer is where `session`,
//! `hls`, `iptv` and panel provisioning are composed. That is what `api` is.
//! The provider half no longer names `AppState` - it takes a
//! `ProviderStreamCtx` - but not naming the root state is not the same as being
//! able to leave.

mod client_stream;
mod custom_video_stream;
mod provisioning_stream;
mod timed_client_stream;
// mod chunked_buffer;
mod active_client_stream;
mod metering_stream;
mod provider_stream;
mod provider_stream_factory;
mod throttled_stream;

// Moved to `utils::network::persist_pipe`; re-exported so api call sites keep
// their existing names.
pub(in crate::api) use self::{
    active_client_stream::*, custom_video_stream::*, metering_stream::*, provider_stream::*,
    provider_stream_factory::*, provisioning_stream::*, throttled_stream::*, timed_client_stream::*,
};
pub(crate) use crate::mpegts::transport_stream_buffer::*;
pub use crate::utils::network::persist_pipe::*;
// Defined by the HTTP client that implements the timeout; re-exported here
// because the streaming layer applies the same default.
pub use crate::utils::network::request::STREAM_IDLE_TIMEOUT;
// Moved to `shared::model`: it is vocabulary shared between the code that
// decides a substitution is needed and the code that performs it. Re-exported
// so api call sites keep their existing names.
pub(crate) use shared::model::CustomVideoStreamType;
// Shared-stream fan-out and its buffer moved to `tuliprox-session`;
// re-exported so api call sites keep their names, module paths included.
pub(crate) use tuliprox_session::streams::buffered_stream;
