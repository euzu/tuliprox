// The `PlaylistProvider` trait returns `impl Future + Send` from a generic impl; proving
// `Send` for that future recurses deeply through the Xtream fetch path and overflows the
// default recursion limit on newer nightlies. Raise it rather than flatten the generics.
#![recursion_limit = "256"]

pub mod capabilities;
pub mod capability_store;
pub mod clock;
pub mod epg;
pub mod error;
pub mod m3u;
pub mod provider;
pub mod redaction;
pub mod stalker;
pub mod xtream;
