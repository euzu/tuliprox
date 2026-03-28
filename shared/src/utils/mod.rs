mod bitset;
mod constants;
mod crypto;
mod default_utils;
mod directed_graph;
pub mod flags;
mod hash_utils;
mod hdhomerun_utils;
mod json_utils;
mod net_utils;
mod number_utils;
mod request;
mod serde_utils;
mod size_utils;
mod string_interner;
mod string_utils;
mod time_utils;

pub use self::{
    constants::*,
    crypto::*,
    default_utils::*,
    directed_graph::*,
    flags::{country_code_to_index, index_to_country_code, FlagEntry, FlagsLoader, DEFAULT_COMPRESSION_LEVEL},
    hash_utils::*,
    hdhomerun_utils::*,
    json_utils::*,
    net_utils::*,
    number_utils::*,
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
    let inner = vec.iter().map(|item| format!("{item}")).collect::<Vec<_>>().join(", ");
    format!("[{inner}]")
}
