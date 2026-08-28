//! Config predicates for the custom-video substitution feature.
//!
//! Read by the streaming layer, the HLS proxy and the provisioning path, so
//! they live with the configuration rather than with any one caller.

use crate::model::AppConfig;

/// `true` when the server may answer a request with a canned video clip.
pub fn is_custom_video_stream_enabled(cfg: &AppConfig) -> bool {
    cfg.config.load().custom_stream_response_enabled
}
