mod bitset;
mod constants;
mod default_utils;
mod directed_graph;
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

pub use self::constants::*;
pub use self::default_utils::*;
pub use self::directed_graph::*;
pub use self::hash_utils::*;
pub use self::hdhomerun_utils::*;
pub use self::json_utils::*;
pub use self::net_utils::*;
pub use self::number_utils::*;
pub use self::request::*;
pub use self::serde_utils::*;
pub use self::size_utils::*;
pub use self::string_interner::*;
pub use self::string_utils::*;
pub use self::time_utils::*;
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
    let inner = vec
        .iter()
        .map(|item| format!("{item}"))
        .collect::<Vec<_>>()
        .join(", ");
    format!("[{inner}]")
}
