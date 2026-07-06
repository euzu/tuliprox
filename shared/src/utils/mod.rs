mod bitset;
mod crypto;
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

pub mod constants;

pub use self::{
    constants::*,
    crypto::*,
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
pub use crate::defaults::*;
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
