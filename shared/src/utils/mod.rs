mod bitset;
mod crypto;
mod directed_graph;
pub mod flags;
mod hash_utils;
mod hdhomerun_utils;
mod json_utils;
mod m3u_url;
mod net_utils;
mod number_utils;
mod recording_filename;
mod request;
mod serde_utils;
mod size_utils;
mod string_interner;
mod string_utils;
mod time_utils;

pub mod constants;

pub use self::{
    constants::*,
    crypto::*,
    directed_graph::*,
    flags::{country_code_to_index, index_to_country_code, FlagEntry, FlagsLoader, DEFAULT_COMPRESSION_LEVEL},
    hash_utils::*,
    hdhomerun_utils::*,
    json_utils::*,
    m3u_url::*,
    net_utils::*,
    number_utils::*,
    recording_filename::*,
    request::*,
    serde_utils::*,
    size_utils::*,
    string_interner::*,
    string_utils::*,
    time_utils::*,
};
use std::fmt::Display;

#[macro_export]
macro_rules! write_if_some {
    ($f:expr, $self:ident, $( $label:literal => $field:ident ),+ $(,)?) => {
        $(
            if let Some(ref val) = $self.$field {
                write!($f, "{}{}", $label, val)?;
            }
        )+
    };
}

pub fn display_vec<T: Display>(vec: &[T]) -> String {
    let mut result = String::from("[");
    for (idx, item) in vec.iter().enumerate() {
        if idx > 0 {
            result.push_str(", ");
        }
        result.push_str(&item.to_string());
    }
    result.push(']');
    result
}

pub fn is_m3u_catchup_session_token(session_token: &str) -> bool {
    session_token.starts_with("m3u-catchup|")
        || session_token.starts_with("catchup|")
        || session_token.contains("|archive|")
        || session_token.contains("|timeshift_abs|")
}

pub fn is_catchup_session_token(session_token: &str) -> bool { is_m3u_catchup_session_token(session_token) }

pub fn contains_ascii_case_insensitive(haystack: &str, needle: &[u8]) -> bool {
    haystack.as_bytes().windows(needle.len()).any(|window| window.eq_ignore_ascii_case(needle))
}
