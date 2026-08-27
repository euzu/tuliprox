//! The reverse-proxy stream settings, resolved once per request.
//!
//! Reads the configuration and nothing else; the streaming layer it configures
//! lives here.

use crate::streams::buffered_stream::MAX_BUFFER_BYTES;
use tuliprox_core::model::AppConfig;

pub struct StreamOptions {
    pub stream_retry: bool,
    pub buffer_enabled: bool,
    pub buffer_size: usize,
    pub buffer_max_bytes: usize,
    pub pipe_provider_stream: bool,
}

/// Constructs a `StreamOptions` object based on the application's reverse proxy configuration.
///
/// This function retrieves streaming-related settings from the configuration:
/// - `stream_retry`: whether retrying the stream is enabled,
/// - `buffer_enabled`: whether stream buffering is enabled,
/// - `buffer_size`: the size of the stream buffer.
///
/// If the reverse proxy or stream settings are not defined, default values are used:
/// - retry: `true`
/// - buffering: `false`
/// - buffer size: `0`
///
/// Additionally, it computes `pipe_provider_stream` as `!stream_retry && !buffer_enabled`.
/// This means direct provider piping is enabled only when retry is disabled and buffering is disabled.
///
/// Returns a `StreamOptions` instance with the resolved configuration.
pub fn get_stream_options(app_config: &AppConfig) -> StreamOptions {
    let (stream_retry, buffer_enabled, buffer_size, buffer_max_bytes) =
        app_config.config.load().reverse_proxy.as_ref().and_then(|reverse_proxy| reverse_proxy.stream.as_ref()).map_or(
            (true, false, 0, MAX_BUFFER_BYTES),
            |stream| {
                let (buffer_enabled, buffer_size, buffer_max_bytes) =
                    stream.buffer.as_ref().map_or((false, 0, MAX_BUFFER_BYTES), |buffer| {
                        let max_bytes = usize::try_from(buffer.max_bytes_mb.saturating_mul(1024 * 1024))
                            .unwrap_or(MAX_BUFFER_BYTES);
                        (buffer.enabled, buffer.size, max_bytes)
                    });
                (stream.retry, buffer_enabled, buffer_size, buffer_max_bytes)
            },
        );
    let pipe_provider_stream = !stream_retry && !buffer_enabled;
    StreamOptions { stream_retry, buffer_enabled, buffer_size, buffer_max_bytes, pipe_provider_stream }
}
