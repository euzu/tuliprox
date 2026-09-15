pub mod atomic_json_store;
mod binary_utils;
pub mod byte_range;
mod clock;
mod compression;
mod crypto_utils;
mod epg_id;
mod epg_parser;
pub mod ffmpeg;
mod file;
mod hls_duration;
mod json_utils;
mod logging;
mod lru_cache;
pub mod network;
mod ordinal;
mod provider_resolve_token;
mod recording_paths;
pub mod request_headers;
pub mod response_compression;
mod step_measure;
mod sys_utils;
mod telegram;
mod time_utils;

#[macro_export]
macro_rules! debug_if_enabled {
    ($fmt:expr, $( $args:expr ),*) => {
        if log::log_enabled!(log::Level::Debug) {
            log::log!(log::Level::Debug, $fmt, $($args),*);
        }
    };

    ($txt:expr) => {
        if log::log_enabled!(log::Level::Debug) {
            log::log!(log::Level::Debug, $txt);
        }
    };
}

#[macro_export]
macro_rules! trace_if_enabled {
    ($fmt:expr, $( $args:expr ),*) => {
        if log::log_enabled!(log::Level::Trace) {
            log::log!(log::Level::Trace, $fmt, $($args),*);
        }
    };

    ($txt:expr) => {
        if log::log_enabled!(log::Level::Trace) {
            log::log!(log::Level::Trace, $txt);
        }
    };
}

#[macro_export]
macro_rules! with {
    (mut $target:expr => $alias:ident $block:block) => {{
        let $alias = &mut $target;
        $block
    }};
    ($target:expr => $alias:ident $block:block) => {{
        let $alias = &$target;
        $block
    }};
}

pub use self::{
    atomic_json_store::*,
    binary_utils::*,
    clock::*,
    compression::*,
    crypto_utils::*,
    epg_id::*,
    epg_parser::*,
    file::*,
    hls_duration::{format_hls_duration_ms, hls_target_duration_secs},
    json_utils::*,
    logging::*,
    lru_cache::*,
    network::*,
    ordinal::*,
    provider_resolve_token::*,
    recording_paths::*,
    step_measure::*,
    sys_utils::*,
    telegram::*,
    time_utils::*,
};
pub use debug_if_enabled;
pub use shared::utils::*;
pub use trace_if_enabled;
pub use with;
