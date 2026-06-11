//! Stalker/Ministra portal client.
//!
//! The client is organized as a set of focused modules that map 1:1 to the public
//! StreamVault-IPTV Stalker client in Kotlin:
//!
//! * [`auth`] — handshake + get-profile, recipe chain, fingerprint detection
//! * [`catalog`] — live / VOD / series paginated fetch
//! * [`epg`] — short EPG, per-channel EPG, bulk streaming EPG
//! * [`playback`] — `create_link` (single command, archive window, batch)
//! * [`session`] — bearer token + cookie jar
//! * [`profile`] — portal account + capabilities snapshot
//! * [`presets`] — MAG device fingerprints
//! * [`recipes`] — handshake fallback chain
//! * [`url_factory`] — sibling endpoint candidates + referer
//! * [`cookie_jar`] — Set-Cookie ingestion
//! * [`cmd_parser`] — `cmd` base64 + URL recovery
//! * [`error`] — typed error variants
//!
//! Tests in this module use a deterministic in-memory `reqwest::Client` substitute
//! (`MockHttp`) when they need to drive the full request/response cycle. The simple
//! `cmd_parser` and `url_factory` helpers are tested without HTTP.

pub mod auth;
pub mod catalog;
pub mod cmd_parser;
pub mod cookie_jar;
pub mod epg;
pub mod error;
pub mod playback;
pub mod presets;
pub mod profile;
pub mod recipes;
pub mod session;
pub mod url_factory;

pub mod client;

pub use client::StalkerApiClient;
pub use error::{StalkerError, StalkerResult};
pub use profile::{StalkerHandshake, StalkerProviderProfile, StalkerRawProviderProfile, StalkerResolvedStream};
pub use session::{StalkerSession, STALKER_SESSION_TTL};
pub use url_factory::{load_url_candidates, portal_referer, StalkerLoadUrl};
