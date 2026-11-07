//! `host::log`. The host rate-limits (100 lines/s) and caps a line at 4 KiB;
//! the excess is dropped and counted, so logging never fails.

use crate::{bindings::tuliprox::plugin::types::LogLevel, sys};

/// Log one line at `level`.
pub fn log(level: LogLevel, msg: impl AsRef<str>) { sys::log(level, msg.as_ref()); }

pub fn trace(msg: impl AsRef<str>) { log(LogLevel::Trace, msg); }
pub fn debug(msg: impl AsRef<str>) { log(LogLevel::Debug, msg); }
pub fn info(msg: impl AsRef<str>) { log(LogLevel::Info, msg); }
pub fn warn(msg: impl AsRef<str>) { log(LogLevel::Warn, msg); }
pub fn error(msg: impl AsRef<str>) { log(LogLevel::Error, msg); }

/// `info!("fetched {n} rows")`: `format!` arguments, host log at `info`.
#[macro_export]
macro_rules! info {
    ($($arg:tt)*) => { $crate::log::info(::std::format!($($arg)*)) };
}
/// Like [`info!`] at `trace`.
#[macro_export]
macro_rules! trace {
    ($($arg:tt)*) => { $crate::log::trace(::std::format!($($arg)*)) };
}
/// Like [`info!`] at `debug`.
#[macro_export]
macro_rules! debug {
    ($($arg:tt)*) => { $crate::log::debug(::std::format!($($arg)*)) };
}
/// Like [`info!`] at `warn`.
#[macro_export]
macro_rules! warn {
    ($($arg:tt)*) => { $crate::log::warn(::std::format!($($arg)*)) };
}
/// Like [`info!`] at `error`.
#[macro_export]
macro_rules! error {
    ($($arg:tt)*) => { $crate::log::error(::std::format!($($arg)*)) };
}
