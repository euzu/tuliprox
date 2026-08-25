pub mod client;

pub use client::*;

// Catchup, archive and token handling moved to `crate::parser::m3u_format`;
// re-exported so `iptv::m3u::X` keeps resolving for this module's callers.
pub use crate::parser::m3u_format::*;
