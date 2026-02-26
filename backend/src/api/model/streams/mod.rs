mod buffered_stream;
mod client_stream;
mod custom_video_stream;
mod provisioning_stream;
mod timed_client_stream;
mod transport_stream_buffer;
// mod chunked_buffer;
mod active_client_stream;
pub mod persist_pipe_stream;
mod provider_stream;
mod provider_stream_factory;
mod shared_stream_manager;
mod throttled_stream;

pub use self::persist_pipe_stream::*;
pub(crate) use self::transport_stream_buffer::*;
pub(in crate::api) use self::{
    active_client_stream::*, custom_video_stream::*, provider_stream::*, provider_stream_factory::*,
    provisioning_stream::*, shared_stream_manager::*, throttled_stream::*, timed_client_stream::*,
};

pub const STREAM_IDLE_TIMEOUT: u64 = 60;
