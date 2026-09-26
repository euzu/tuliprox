use crate::{
    model::stalker::{StalkerPlaybackDescriptorDto, StalkerPortalCapabilitiesDto, StalkerStreamKind},
    utils::{arc_str_null_is_none_option_serde, arc_str_serde, arc_str_vec_serde, Internable},
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// A single Stalker/Ministra playlist item persisted in the B+Tree store.
/// Mirrors the `StreamVault` `StalkerPlaylistItem` Kotlin data class — all
/// Stalker-specific fields are captured so the runtime can decide between
/// pre-resolved and on-demand `create_link` calls.
///
/// IMPORTANT: this struct is persisted via positional `MessagePack`
/// (`rmp_serde::to_vec` in `binary_serialize`). Fields are encoded by
/// position, not name — `skip_serializing_if` would shift every following
/// field on read and corrupt the record. Optional fields use `#[serde(default)]`
/// only, and new fields may only be APPENDED (never inserted/reordered).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StalkerPlaylistItem {
    pub stream_id: u32,
    #[serde(with = "arc_str_serde")]
    pub name: Arc<str>,
    pub category_id: u32,
    #[serde(with = "arc_str_serde")]
    pub category_name: Arc<str>,
    pub number: u32,
    #[serde(default)]
    pub logo_url: Option<Arc<str>>,
    #[serde(default)]
    pub epg_channel_id: Option<Arc<str>>,
    /// Resolved stream URL. Empty until playback is materialized, then filled
    /// with the real upstream http/https URL.
    #[serde(with = "arc_str_serde")]
    pub stream_url: Arc<str>,
    /// Live/VOD/Series/Archive classification.
    pub stream_kind: StalkerStreamKind,
    /// Raw `cmd` string returned by the portal (`ffmpeg <url> <ext>`).
    #[serde(with = "arc_str_serde")]
    pub cmd: Arc<str>,
    #[serde(default, with = "arc_str_null_is_none_option_serde")]
    pub container_extension: Option<Arc<str>>,
    #[serde(default)]
    pub plot: Option<Arc<str>>,
    #[serde(default)]
    pub cast: Option<Arc<str>>,
    #[serde(default)]
    pub director: Option<Arc<str>>,
    #[serde(default)]
    pub genre: Option<Arc<str>>,
    #[serde(default)]
    pub release_date: Option<Arc<str>>,
    #[serde(default)]
    pub rating: f32,
    #[serde(default)]
    pub tmdb_id: Option<i64>,
    #[serde(default)]
    pub backdrop_url: Option<Arc<str>>,
    /// Unix timestamp in seconds when the item was first persisted.
    #[serde(default)]
    pub added_at: i64,
    #[serde(default)]
    pub is_adult: bool,
    /// `true` for series root items; episodes are persisted under
    /// `stalker_episode.db` and reference their parent `series_id`.
    #[serde(default)]
    pub is_series: bool,
    /// Playback descriptor (primary mode + ordered cmd candidates + capabilities).
    /// When `None`, the runtime treats the item as a plain direct URL.
    #[serde(default)]
    pub playback_descriptor: Option<StalkerPlaybackDescriptorDto>,
    /// Per-item capability flags lifted from the portal — let the runtime
    /// skip `create_link` retries when the portal clearly does not support
    /// timeshift or temp-link playback.
    #[serde(default)]
    pub archive_available: bool,
    #[serde(default)]
    pub allow_local_timeshift: bool,
    #[serde(default)]
    pub allow_local_pvr: bool,
    #[serde(default)]
    pub allow_remote_pvr: bool,
    #[serde(default)]
    pub nginx_secure_link: bool,
    #[serde(default)]
    pub flussonic_tmp_link: bool,
    #[serde(default)]
    pub wowza_tmp_link: bool,
    #[serde(default)]
    pub use_http_tmp_link: bool,
    /// Series linkage (only set for episode items).
    #[serde(default)]
    pub series_id: Option<u32>,
    #[serde(default)]
    pub season_id: Option<u32>,
    #[serde(default)]
    pub episode_id: Option<u32>,
}

impl Default for StalkerPlaylistItem {
    fn default() -> Self {
        Self {
            stream_id: 0,
            name: "".intern(),
            category_id: 0,
            category_name: "".intern(),
            number: 0,
            logo_url: None,
            epg_channel_id: None,
            stream_url: "".intern(),
            stream_kind: StalkerStreamKind::Live,
            cmd: "".intern(),
            container_extension: None,
            plot: None,
            cast: None,
            director: None,
            genre: None,
            release_date: None,
            rating: 0.0,
            tmdb_id: None,
            backdrop_url: None,
            added_at: 0,
            is_adult: false,
            is_series: false,
            playback_descriptor: None,
            archive_available: false,
            allow_local_timeshift: false,
            allow_local_pvr: false,
            allow_remote_pvr: false,
            nginx_secure_link: false,
            flussonic_tmp_link: false,
            wowza_tmp_link: false,
            use_http_tmp_link: false,
            series_id: None,
            season_id: None,
            episode_id: None,
        }
    }
}

impl StalkerPlaylistItem {
    /// Whether this item is a series root (no individual playback URL).
    pub fn is_series_root(&self) -> bool { self.is_series }

    /// Whether this item supports archive/timeshift playback.
    pub fn supports_archive(&self) -> bool {
        self.archive_available || self.allow_local_timeshift || self.allow_local_pvr || self.allow_remote_pvr
    }

    /// Whether playback requires a temp-link (nginx/flussonic/wowza/http).
    pub fn uses_temp_link(&self) -> bool {
        self.nginx_secure_link || self.flussonic_tmp_link || self.wowza_tmp_link || self.use_http_tmp_link
    }
}

/// A season entry that groups a list of episodes. Persisted alongside series items.
///
/// NOTE: persisted via positional `MessagePack` — no `skip_serializing_if`
/// (see `StalkerPlaylistItem`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct StalkerSeasonItem {
    pub series_id: u32,
    pub season_number: i32,
    #[serde(with = "arc_str_serde")]
    pub name: Arc<str>,
    #[serde(default)]
    pub cover_url: Option<Arc<str>>,
    #[serde(with = "arc_str_vec_serde", default)]
    pub episodes: Vec<Arc<str>>,
}

/// Lightweight episode record that keeps the actual `StalkerPlaylistItem`
/// in the live/episode store and only embeds the season-level summary here.
///
/// NOTE: persisted via positional `MessagePack` — no `skip_serializing_if`
/// (see `StalkerPlaylistItem`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StalkerEpisodeIndex {
    pub episode_id: u64,
    pub series_id: u32,
    pub season_number: i32,
    pub episode_number: i32,
    #[serde(with = "arc_str_serde")]
    pub title: Arc<str>,
    #[serde(default, with = "arc_str_null_is_none_option_serde")]
    pub container_extension: Option<Arc<str>>,
    #[serde(default)]
    pub added_at: i64,
}

impl StalkerEpisodeIndex {
    pub fn is_empty(&self) -> bool {
        self.title.trim().is_empty()
            && self.container_extension.as_deref().map(str::trim).is_none_or(str::is_empty)
            && self.added_at == 0
    }
}

impl StalkerPortalCapabilitiesDto {
    /// Merge the item-level capability flags into this portal descriptor.
    pub fn merge_into(&mut self, item: &StalkerPlaylistItem) {
        if item.archive_available {
            self.archive_available = true;
        }
        if item.allow_local_timeshift {
            self.allow_local_timeshift = true;
        }
        if item.allow_local_pvr {
            self.allow_local_pvr = true;
        }
        if item.allow_remote_pvr {
            self.allow_remote_pvr = true;
        }
        if item.nginx_secure_link {
            self.nginx_secure_link = true;
        }
        if item.flussonic_tmp_link {
            self.flussonic_temporary_link = true;
        }
        if item.wowza_tmp_link {
            self.wowza_temporary_link = true;
        }
        if item.use_http_tmp_link {
            self.use_http_temporary_link = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::stalker::StalkerPlaybackMode;

    fn make_test_item() -> StalkerPlaylistItem {
        StalkerPlaylistItem {
            stream_id: 42,
            name: "Channel 42".intern(),
            category_id: 7,
            category_name: "News".intern(),
            number: 1,
            logo_url: Some("https://cdn.example/logo.png".intern()),
            epg_channel_id: Some("ch-42".intern()),
            stream_url: "https://stream.example/42.ts".intern(),
            stream_kind: StalkerStreamKind::Live,
            cmd: "ffmpeg https://stream.example/42.ts ts".intern(),
            container_extension: Some("ts".intern()),
            rating: 0.0,
            added_at: 1_700_000_000,
            archive_available: true,
            allow_local_timeshift: true,
            ..Default::default()
        }
    }

    #[test]
    fn item_serde_round_trip_preserves_all_fields() {
        let item = make_test_item();
        let json = serde_json::to_string(&item).expect("serialize");
        let decoded: StalkerPlaylistItem = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(item, decoded);
    }

    #[test]
    fn item_supports_archive_reflects_capability_flags() {
        let mut item = make_test_item();
        assert!(item.supports_archive());
        item.archive_available = false;
        item.allow_local_timeshift = false;
        item.allow_local_pvr = false;
        item.allow_remote_pvr = false;
        assert!(!item.supports_archive());
    }

    #[test]
    fn item_uses_temp_link_when_any_flag_set() {
        let mut item = make_test_item();
        item.archive_available = false;
        item.allow_local_timeshift = false;
        item.allow_local_pvr = false;
        item.allow_remote_pvr = false;
        assert!(!item.uses_temp_link());
        item.nginx_secure_link = true;
        assert!(item.uses_temp_link());
    }

    #[test]
    fn episode_index_is_empty_when_only_ids_present() {
        let episode = StalkerEpisodeIndex {
            episode_id: 1,
            series_id: 100,
            season_number: 1,
            episode_number: 1,
            title: "Pilot".intern(),
            container_extension: None,
            added_at: 0,
        };
        assert!(!episode.is_empty());
        let empty = StalkerEpisodeIndex {
            episode_id: 2,
            series_id: 100,
            season_number: 1,
            episode_number: 2,
            title: "".intern(),
            container_extension: None,
            added_at: 0,
        };
        assert!(empty.is_empty());
    }

    #[test]
    fn descriptor_merge_into_captures_item_capabilities() {
        let mut descriptor = StalkerPortalCapabilitiesDto::default();
        let mut item = make_test_item();
        item.nginx_secure_link = true;
        item.flussonic_tmp_link = true;
        item.playback_descriptor = Some(StalkerPlaybackDescriptorDto {
            primary_mode: StalkerPlaybackMode::TempLinkNginx,
            ..Default::default()
        });
        descriptor.merge_into(&item);
        assert!(descriptor.archive_available);
        assert!(descriptor.nginx_secure_link);
        assert!(descriptor.flussonic_temporary_link);
    }
}
