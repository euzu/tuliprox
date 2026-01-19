mod sys_utils;
mod compression;
mod file;
mod network;
mod crypto_utils;
mod step_measure;
mod logging;
mod trakt;
mod json_utils;
mod binary_utils;
mod telegram;
mod geoip;
mod db_viewer;

pub use self::binary_utils::*;
pub use self::logging::*;
pub use self::trakt::*;
pub use self::telegram::*;
pub use self::geoip::*;
pub use self::db_viewer::*;
pub use shared::utils::*;

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

pub use debug_if_enabled;
pub use trace_if_enabled;
pub use with;

pub use self::json_utils::*;
pub use self::sys_utils::*;
pub use self::compression::*;
pub use self::file::*;
pub use self::network::*;
pub use self::crypto_utils::*;
pub use self::step_measure::*;
