mod client_stream;
mod custom_video_stream;
mod provisioning_stream;
mod timed_client_stream;
// mod chunked_buffer;
mod active_client_stream;
mod provider_stream;
mod provider_stream_factory;
mod metering_stream;
mod throttled_stream;

// Moved to `utils::network::persist_pipe`; re-exported so api call sites keep
// their existing names.
pub use crate::utils::network::persist_pipe::*;
pub(crate) use crate::mpegts::transport_stream_buffer::*;

// Moved to `shared::model`: it is vocabulary shared between the code that
// decides a substitution is needed and the code that performs it. Re-exported
// so api call sites keep their existing names.
pub(crate) use shared::model::CustomVideoStreamType;

// Shared-stream fan-out and its buffer moved to `tuliprox-session`;
// re-exported so api call sites keep their names, module paths included.
pub(crate) use tuliprox_session::streams::buffered_stream;
pub(in crate::api) use self::{
    active_client_stream::*, custom_video_stream::*, metering_stream::*, provider_stream::*,
    provider_stream_factory::*, provisioning_stream::*,
    throttled_stream::*, timed_client_stream::*,
};

// Defined by the HTTP client that implements the timeout; re-exported here
// because the streaming layer applies the same default.
pub use crate::utils::network::request::STREAM_IDLE_TIMEOUT;
