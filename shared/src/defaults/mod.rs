//! Default-value helpers grouped by domain.
//!
//! Items are re-exported at the crate root via `shared::utils::*` (see
//! `shared/src/utils/mod.rs`) so all existing `use shared::utils::default_xxx;`
//! imports keep working unchanged.
//!
//! The 14 leaf files in this directory are flat by external-service name
//! (auth, epg, hdhomerun, hls, library, `media_server`, metadata, monitoring,
//! network, paths, primitives, `stream_history`, tmdb, trakt). To answer
//! "what is the default for HLS?" in one grep, the same items are also
//! re-exported under three logical groups:
//!
//! - [`config`]  — streaming-side defaults: auth, hls, hdhomerun, library, `media_server`
//! - [`integrations`]  — external-service defaults: epg, metadata, tmdb, trakt
//! - [`runtime`]  — runtime/infrastructure defaults: monitoring, network, paths, primitives, `stream_history`
//!
//! New defaults can be added to either the flat layout or a group module;
//! both are re-exported at the crate root.

/// Generates `default_*` / `is_default_*` pairs for value types.
///
/// Numeric arms (`u8`/`u16`/`u32`/`usize`/`i64`/`u64`) emit `const fn` returning
/// the value directly. The `str` arm emits `String`-returning defaults so
/// existing call sites that store into `String`-typed config fields keep
/// working unchanged (one heap allocation per `default_*` invocation;
/// defaults are read at config-load time, not in a hot path).
///
/// Add a new arm (e.g. `byte_size`) when a non-numeric type starts repeating
/// the same `default_*` + `is_default_*` shape.
#[macro_export]
macro_rules! default_eq_fns {
    // String-typed defaults (e.g. duration strings "30s", "1h", "7d").
    // This arm MUST come first — the numeric arm below would otherwise match
    // `str, "30s"` and emit `pub const fn ...() -> str { ... }` (str is
    // unsized, so the generated code fails to compile).
    ($( $default_fn:ident, $is_default_fn:ident, str, $value:literal; )* ) => {
        $(
            pub fn $default_fn() -> String { $value.to_string() }
            pub fn $is_default_fn(v: &String) -> bool { v == &$default_fn() }
        )*
    };
    // Numeric: literal default folds into a const.
    ($( $default_fn:ident, $is_default_fn:ident, $ty:ty, $value:expr; )* ) => {
        $(
            pub const fn $default_fn() -> $ty { $value }
            pub const fn $is_default_fn(v: &$ty) -> bool { *v == $default_fn() }
        )*
    };
}

/// Generates `Display` + `FromStr` for a snake_case-renamed string enum.
/// String labels must equal the `#[serde(rename_all = "snake_case")]` form so
/// the wire format, `Display`, and `FromStr` agree on a single canonical name.
#[macro_export]
macro_rules! impl_str_enum {
    ( $enum:ty, $err_label:expr, $( $variant:ident => $str:literal ),+ $(,)? ) => {
        impl std::fmt::Display for $enum {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                match self {
                    $( Self::$variant => f.write_str($str), )+
                }
            }
        }
        impl std::str::FromStr for $enum {
            type Err = String;
            fn from_str(value: &str) -> Result<Self, Self::Err> {
                match value {
                    $( $str => Ok(Self::$variant), )+
                    _ => Err(format!("Unknown {}: {value}", $err_label)),
                }
            }
        }
    };
}

mod auth;
mod epg;
mod hdhomerun;
mod hls;
mod library;
mod media_server;
mod metadata;
mod monitoring;
mod network;
mod paths;
mod primitives;
mod qos;
mod stream_history;
mod tmdb;
mod trakt;

pub use self::{
    auth::*, epg::*, hdhomerun::*, hls::*, library::*, media_server::*, metadata::*, monitoring::*, network::*,
    paths::*, primitives::*, qos::*, stream_history::*, tmdb::*, trakt::*,
};

/// Streaming-side defaults (auth, hls, hdhomerun, library, `media_server`).
/// Re-exports the flat items under a logical group so callers asking
/// "what is the default for HLS?" can grep one place.
pub mod config {
    pub use super::{auth::*, hdhomerun::*, hls::*, library::*, media_server::*};
}

/// External-service defaults (epg, metadata, tmdb, trakt).
pub mod integrations {
    pub use super::{epg::*, metadata::*, tmdb::*, trakt::*};
}

/// Runtime/infrastructure defaults (monitoring, network, paths, primitives, `QoS`, stream history).
pub mod runtime {
    pub use super::{monitoring::*, network::*, paths::*, primitives::*, qos::*, stream_history::*};
}
