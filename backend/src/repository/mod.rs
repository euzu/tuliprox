mod storage;
mod target_id_mapping;
// A B+Tree-backed IPv4 lookup and a CLI dumper for the repository's databases.
// Both read repository storage directly, so they belong here rather than in
// `utils`, which must not depend on this layer.
mod network_access;
mod db_viewer;
mod geoip;
pub mod playlist_mem_cache;
mod metadata_retry_record;
mod startup_migration;
mod playlist_repository;
mod m3u_repository;
mod xtream_repository;
mod epg_repository;
mod strm_repository;
mod m3u_playlist_iterator;
mod xtream_playlist_iterator;
mod user_repository;
pub mod storage_const;
mod playlist_scratch;
mod playlist_source;
mod library_repository;
mod live_stream_metadata_repository;
mod alias_repository;
mod playlist_stream;
mod provider_dns_repository;
mod stream_history;
mod qos_snapshot_repository;
pub mod identity_registry;
pub mod recording_rule_repository;
pub mod stalker_repository;
pub mod stalker_generation_repository;

pub use storage::*;
pub use target_id_mapping::*;
// The B+Tree storage engine is its own package. Aliased under its historical
// module name so every `crate::repository::bplustree::X` path keeps resolving.
pub use tuliprox_btree as bplustree;
pub use tuliprox_btree::*;
pub use network_access::*;
pub use db_viewer::*;
pub use geoip::*;
pub use metadata_retry_record::*;
pub use playlist_mem_cache::*;
pub use startup_migration::*;
pub use playlist_repository::*;
pub use m3u_repository::*;
pub use xtream_repository::*;
pub use epg_repository::*;
pub use strm_repository::*;
pub use m3u_playlist_iterator::*;
pub use xtream_playlist_iterator::*;
pub use user_repository::*;
pub use storage_const::*;
pub use alias_repository::*;
pub use playlist_source::*;
pub use library_repository::*;
pub(crate) use live_stream_metadata_repository::{
    load_input_live_bitrate_bps, persist_input_live_bitrate_bps, LiveBitratePersistenceOutcome,
};
pub use playlist_stream::*;
pub use provider_dns_repository::*;
pub use stream_history::*;
pub use qos_snapshot_repository::*;
pub use stalker_repository::*;
