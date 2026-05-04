//! Media-server anti-corruption layer.
//!
//! This module keeps Emby/Jellyfin/Plex wire DTOs and transport concerns at the
//! Source Acquisition boundary. Playlist Curation and Stream Brokerage should
//! depend on the typed concepts exported here instead of provider-specific DTOs.

pub mod catalog;
pub mod client;
pub mod emby;
pub mod errors;
pub mod import;
pub mod jellyfin;
pub mod playback;
pub mod playlist_mapper;
pub mod plex;
pub mod redaction;
pub mod types;

#[cfg(test)]
pub mod test_fixtures;

pub use catalog::*;
pub use client::*;
pub use errors::*;
pub use import::*;
pub use playback::*;
pub use playlist_mapper::*;
pub use redaction::*;
pub use types::*;
