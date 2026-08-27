//! M3U catchup and archive URL handling.
//!
//! Builds and decodes the provider URLs an M3U playlist carries: catchup
//! templates, the signed catchup token, and Flussonic archive paths. Pure
//! format work with no dependency on anything above it, which is why the
//! playlist iterator in the repository can use it directly.

mod archive;
mod catchup;
mod catchup_token;

pub use archive::*;
pub use catchup::*;
pub use catchup_token::*;
