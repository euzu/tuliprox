use std::error::Error;
use std::fmt::{Display, Formatter, Result};

#[macro_export]
macro_rules! get_errors_notify_message {
    ($errors:expr, $size:expr) => {
        if $errors.is_empty() {
            None
        } else {
            let text = $errors
                .iter()
                .filter(|&err| err.kind == $crate::error::TuliproxErrorKind::Notify)
                .map(|err| err.message.as_str())
                .collect::<Vec<&str>>()
                .join("\r\n");
            if $size > 0 && text.len() > std::cmp::max($size - 3, 3) {
                Some(format!("{}...", text.get(0..$size).unwrap()))
            } else {
                Some(text)
            }
        }
    };
}

pub use get_errors_notify_message;

#[macro_export]
macro_rules! notify_err {
    ($($arg:tt)*) => {
        $crate::error::TuliproxError::new($crate::error::TuliproxErrorKind::Notify, format!($($arg)*))
    };
}

pub use notify_err;

#[macro_export]
macro_rules! notify_err_res {
    ($($arg:tt)*) => {
        Err($crate::error::TuliproxError::new($crate::error::TuliproxErrorKind::Notify, format!($($arg)*)))
    };
}

pub use notify_err_res;


#[macro_export]
macro_rules! info_err {
    // This matches any arguments (format string + variables) and forwards them
    // to format!, then wraps them in your Error constructor.
    ($($arg:tt)*) => {
        $crate::error::TuliproxError::new($crate::error::TuliproxErrorKind::Info, format!($($arg)*))
    };
}

pub use info_err;

#[macro_export]
macro_rules! info_err_res {
    // This matches any arguments (format string + variables) and forwards them
    // to format!, then wraps them in your Error constructor.
    ($($arg:tt)*) => {
        Err($crate::error::TuliproxError::new($crate::error::TuliproxErrorKind::Info, format!($($arg)*)))
    };
}

pub use info_err_res;


#[macro_export]
macro_rules! handle_tuliprox_error_result_list {
    ($kind:expr, $result: expr) => {
        let errors = $result
            .filter_map(|result| {
                if let Err(err) = result {
                    Some(err.to_string())
                } else {
                    None
                }
            })
            .collect::<Vec<String>>();
        if !&errors.is_empty() {
            return Err($crate::error::TuliproxError::new($kind, errors.join("\n")));
        }
    }
}

pub use handle_tuliprox_error_result_list;

#[macro_export]
macro_rules! handle_tuliprox_error_result {
    ($kind:expr, $result: expr) => {
        if let Err(err) = $result {
            return Err($crate::error::TuliproxError::new($kind, err.to_string()));
        }
    }
}

pub use handle_tuliprox_error_result;


#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum TuliproxErrorKind {
    // do not send with messaging
    Info,
    Notify, // send with messaging
}

#[derive(Debug)]
pub struct TuliproxError {
    pub kind: TuliproxErrorKind,
    pub message: String,
}

impl TuliproxError {
    pub const fn new(kind: TuliproxErrorKind, message: String) -> Self {
        Self { kind, message }
    }
}

impl Display for TuliproxError {
    fn fmt(&self, f: &mut Formatter) -> Result {
        write!(f, "Tuliprox error: {}", self.message)
    }
}

impl Error for TuliproxError {}

pub fn to_io_error<E>(err: E) -> std::io::Error
where
    E: std::error::Error,
{ std::io::Error::other(err.to_string()) }

pub fn str_to_io_error(err: &str) -> std::io::Error {
    std::io::Error::other(err.to_string())
}

pub fn string_to_io_error(err: String) -> std::io::Error {
    std::io::Error::other(err)
}
