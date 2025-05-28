mod string_utils;
mod json_utils;
mod default_utils;
mod size_utils;
mod sys_utils;
mod hash_utils;
mod compression;
mod file;
mod network;
mod bincode_utils;
mod time_utils;
mod crypto_utils;
mod constants;
mod step_measure;
pub mod logging;
pub mod trakt;

#[macro_export]
macro_rules! debug_if_enabled {
    ($fmt:expr, $( $args:expr ),*) => {
        if log::log_enabled!(log::Level::Debug) {
            log::log!(log::Level::Debug, $fmt, $($args),*);
        }
    };

    ($txt:expr) => {
        if log::log_enabled!(log::Level::Debug) {
            log::log!(Level::Debug, $txt);
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
            log::log!(Level::Trace, $txt);
        }
    };
}

pub use debug_if_enabled;
pub use trace_if_enabled;

pub use self::default_utils::*;
pub use self::string_utils::*;
pub use self::json_utils::*;
pub use self::size_utils::*;
pub use self::sys_utils::*;
pub use self::hash_utils::*;
pub use self::compression::*;
pub use self::file::*;
pub use self::network::*;
pub use self::bincode_utils::*;
pub use self::time_utils::*;
pub use self::crypto_utils::*;
pub use self::constants::*;
pub use self::step_measure::*;
