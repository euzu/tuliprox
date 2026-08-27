//! Stalker/Ministra item parsers.
//!
//! Converts the raw portal JSON returned by the Stalker client
//! ([`StalkerRawItem`]/[`StalkerRawSeriesItem`]/[`StalkerRawSeriesDetails`]) into the
//! persisted [`StalkerPlaylistItem`] shape used by the B+Tree and the runtime
//! [`PlaylistItem`] headers.
//!
//! The parsers in this file are intentionally side-effect-free: they do not open network
//! connections, do not touch the cookie jar and do not write to disk. Catalog fetching
//! lives in the Stalker API client; disk persistence and pre-resolve orchestration live
//! in the processor.

#![allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]

use super::catalog::{
    StalkerCategory, StalkerRawItem, StalkerRawItemInfo, StalkerRawSeriesDetails, StalkerRawSeriesEpisode,
    StalkerRawSeriesItem, StalkerRawSeriesSeason,
};
use log::{info, warn};
use shared::{
    model::{
        stalker::{StalkerCommandVariantDto, StalkerPlaybackDescriptorDto, StalkerPlaybackMode, StalkerStreamKind},
        stalker_item::StalkerPlaylistItem,
    },
    utils::{fnv1a_32, stable_episode_storage_id, Internable},
};
use std::{collections::HashSet, sync::Arc};

fn intern_optional(s: Option<String>) -> Option<Arc<str>> { s.filter(|v| !v.is_empty()).map(Internable::intern) }

fn intern_optional_value(s: Option<&String>) -> Option<Arc<str>> {
    s.filter(|v| !v.is_empty()).map(|v| Internable::intern(v.clone()))
}

#[derive(Debug, Copy, Clone, Default, PartialEq, Eq)]
pub struct StalkerTempLinkFlags(u8);

impl StalkerTempLinkFlags {
    const NGINX_SECURE_LINK: u8 = 1 << 0;
    const FLUSSONIC_TMP_LINK: u8 = 1 << 1;
    const WOWZA_TMP_LINK: u8 = 1 << 2;
    const USE_HTTP_TMP_LINK: u8 = 1 << 3;

    fn has_nginx_secure_link(self) -> bool { self.0 & Self::NGINX_SECURE_LINK != 0 }

    fn has_flussonic_tmp_link(self) -> bool { self.0 & Self::FLUSSONIC_TMP_LINK != 0 }

    fn has_wowza_tmp_link(self) -> bool { self.0 & Self::WOWZA_TMP_LINK != 0 }
}

impl From<&StalkerRawItem> for StalkerTempLinkFlags {
    fn from(raw: &StalkerRawItem) -> Self {
        Self::from([
            raw.nginx_secure_link.unwrap_or(false),
            raw.flussonic_tmp_link.unwrap_or(false),
            raw.wowza_tmp_link.unwrap_or(false),
            raw.use_http_tmp_link.unwrap_or(false),
        ])
    }
}

impl From<[bool; 4]> for StalkerTempLinkFlags {
    fn from(flags: [bool; 4]) -> Self {
        let mut bits = 0_u8;
        if flags[0] {
            bits |= Self::NGINX_SECURE_LINK;
        }
        if flags[1] {
            bits |= Self::FLUSSONIC_TMP_LINK;
        }
        if flags[2] {
            bits |= Self::WOWZA_TMP_LINK;
        }
        if flags[3] {
            bits |= Self::USE_HTTP_TMP_LINK;
        }
        Self(bits)
    }
}

/// Build a `StalkerPlaylistItem` from a live/VOD row plus its category. The mapping is
/// forgiving: every field on the source is optional and is converted via `.unwrap_or_*`
/// helpers so a partial row still produces a usable playlist item.
pub fn map_stalker_to_playlist_item(
    raw: &StalkerRawItem,
    category: Option<&StalkerCategory>,
    stream_kind: StalkerStreamKind,
    added_at: i64,
) -> StalkerPlaylistItem {
    let name = raw.display_name();
    let category_id = raw
        .category_id()
        .and_then(|s| s.parse::<u32>().ok())
        .or_else(|| category.and_then(|c| c.id.parse::<u32>().ok()))
        .unwrap_or(0);
    let category_name = category.map(|c| c.title.clone()).unwrap_or_default();
    let number = raw.number.as_deref().and_then(|s| s.parse::<u32>().ok()).unwrap_or(0);
    let stream_id = raw.stream_id().unwrap_or(0);
    let logo = raw.logo.clone().or_else(|| raw.stream_icon.clone());
    let cmd = raw.cmd.clone().unwrap_or_default();

    let info = raw.info.clone();
    StalkerPlaylistItem {
        stream_id,
        name: Internable::intern(name.to_string()),
        category_id,
        category_name: Internable::intern(category_name),
        number,
        logo_url: intern_optional(logo),
        epg_channel_id: intern_optional(raw.epg_channel_id.clone()),
        // `stream_url` is filled in by the processor — the raw item does not carry a
        // playable URL, only the `cmd` token. The processor either runs `create_link`
        // (pre-resolve) or stores the cmd-derived marker for runtime re-resolve.
        stream_url: Internable::intern(String::new()),
        stream_kind,
        cmd: Internable::intern(cmd),
        container_extension: intern_optional(raw.container_extension.clone()),
        plot: extract_info_text(info.as_ref(), |i| i.plot.clone()),
        cast: extract_info_text(info.as_ref(), |i| i.cast.clone()),
        director: extract_info_text(info.as_ref(), |i| i.director.clone()),
        genre: extract_info_text(info.as_ref(), |i| i.genre.clone()),
        release_date: extract_info_text(info.as_ref(), |i| i.releasedate.clone()),
        rating: info.as_ref().and_then(|i| i.rating).unwrap_or(0.0),
        tmdb_id: info.as_ref().and_then(|i| i.tmdb_id.clone()).and_then(|s| s.parse::<i64>().ok()),
        backdrop_url: info
            .as_ref()
            .and_then(|i| i.backdrop.clone())
            .and_then(|list| list.first().cloned())
            .filter(|s| !s.is_empty())
            .map(Internable::intern),
        added_at,
        is_adult: false,
        is_series: false,
        playback_descriptor: build_descriptor_from_raw(raw),
        archive_available: raw.tv_archive.unwrap_or(false),
        allow_local_timeshift: raw.allow_local_timeshift.unwrap_or(false),
        allow_local_pvr: raw.allow_pvr.unwrap_or(false) || raw.pvr.unwrap_or(false),
        allow_remote_pvr: raw.pvr_shift.unwrap_or(false) || raw.pvr_time_shift.unwrap_or(false),
        nginx_secure_link: raw.nginx_secure_link.unwrap_or(false),
        flussonic_tmp_link: raw.flussonic_tmp_link.unwrap_or(false),
        wowza_tmp_link: raw.wowza_tmp_link.unwrap_or(false),
        use_http_tmp_link: raw.use_http_tmp_link.unwrap_or(false),
        series_id: raw.series_id.as_ref().and_then(|s| s.parse::<u32>().ok()),
        season_id: None,
        episode_id: None,
    }
}

fn extract_info_text<F>(info: Option<&StalkerRawItemInfo>, field: F) -> Option<Arc<str>>
where
    F: FnOnce(&StalkerRawItemInfo) -> Option<String>,
{
    info.and_then(field).map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).map(Internable::intern)
}

fn build_descriptor_from_raw(raw: &StalkerRawItem) -> Option<StalkerPlaybackDescriptorDto> {
    let cmd = raw.cmd.clone().filter(|s| !s.is_empty())?;
    let mode = playback_mode_from_flags(StalkerTempLinkFlags::from(raw));
    let candidates = vec![StalkerCommandVariantDto { cmd, playback_mode: mode, source_key: None, priority: 0 }];
    Some(StalkerPlaybackDescriptorDto { primary_mode: mode, candidates, capabilities: None })
}

/// Map the raw Stalker temp-link capability flags to a canonical `StalkerPlaybackMode`.
/// Precedence (most specific first):
/// 1. `nginx_secure_link` -> `TempLinkNginx`
/// 2. `flussonic_tmp_link` -> `TempLinkFlussonic`
/// 3. `wowza_tmp_link` -> `TempLinkWowza`
/// 4. `use_http_tmp_link` -> `DirectUrl` (HTTP temp link is just a normal URL with TTL)
/// 5. otherwise `DirectUrl`
pub fn playback_mode_from_flags(flags: StalkerTempLinkFlags) -> StalkerPlaybackMode {
    if flags.has_nginx_secure_link() {
        StalkerPlaybackMode::TempLinkNginx
    } else if flags.has_flussonic_tmp_link() {
        StalkerPlaybackMode::TempLinkFlussonic
    } else if flags.has_wowza_tmp_link() {
        StalkerPlaybackMode::TempLinkWowza
    } else {
        // use_http_tmp_link resolves to a direct http(s) URL — it does not need a
        // distinct adapter, so it maps to DirectUrl like the unflagged case.
        StalkerPlaybackMode::DirectUrl
    }
}

/// Build a `StalkerPlaylistItem` from a series root row. Series roots are stored with
/// `is_series=true` and `series_id` set so the runtime can fan out to per-season details.
pub fn map_stalker_series_root(
    raw: &StalkerRawSeriesItem,
    category: Option<&StalkerCategory>,
    added_at: i64,
) -> StalkerPlaylistItem {
    let name = raw.display_name();
    let category_id = raw
        .category_id
        .as_deref()
        .and_then(|s| s.parse::<u32>().ok())
        .or_else(|| category.and_then(|c| c.id.parse::<u32>().ok()))
        .unwrap_or(0);
    let category_name = category.map(|c| c.title.clone()).unwrap_or_default();
    let number = raw.number.as_deref().and_then(|s| s.parse::<u32>().ok()).unwrap_or(0);
    let series_id = raw.id_string().as_deref().and_then(|s| s.parse::<u32>().ok()).unwrap_or(0);
    let logo = raw.logo.clone().or_else(|| raw.cover.clone());

    StalkerPlaylistItem {
        stream_id: series_id,
        name: Internable::intern(name.to_string()),
        category_id,
        category_name: Internable::intern(category_name),
        number,
        logo_url: intern_optional(logo),
        epg_channel_id: None,
        stream_url: Internable::intern(String::new()),
        stream_kind: StalkerStreamKind::Episode,
        cmd: Internable::intern(String::new()),
        container_extension: None,
        plot: intern_optional_value(raw.plot.as_ref()),
        cast: intern_optional_value(raw.cast.as_ref()),
        director: intern_optional_value(raw.director.as_ref()),
        genre: intern_optional_value(raw.genre.as_ref()),
        release_date: intern_optional_value(raw.releasedate.as_ref()),
        rating: raw.rating.unwrap_or(0.0),
        tmdb_id: raw.tmdb_id.as_deref().and_then(|s| s.parse::<i64>().ok()),
        backdrop_url: raw
            .backdrop
            .as_ref()
            .and_then(|list| list.first().cloned())
            .filter(|s| !s.is_empty())
            .map(Internable::intern),
        added_at,
        is_adult: false,
        is_series: true,
        playback_descriptor: None,
        archive_available: false,
        allow_local_timeshift: false,
        allow_local_pvr: false,
        allow_remote_pvr: false,
        nginx_secure_link: false,
        flussonic_tmp_link: false,
        wowza_tmp_link: false,
        use_http_tmp_link: false,
        series_id: Some(series_id),
        season_id: None,
        episode_id: None,
    }
}

/// Walk a series-details response and produce a list of episode `StalkerPlaylistItem`s
/// (one per episode across all seasons). The `series_id` is taken from the parent row.
///
/// `used_episode_ids` is the set of storage ids already assigned within the current
/// snapshot batch — pass one set for the whole refresh so storage-id collisions are
/// detected and re-salted deterministically across all series.
// The caller threads one set through a whole refresh; generalizing over the
// hasher would buy nothing and complicate that call site.
#[allow(clippy::implicit_hasher)]
pub fn map_stalker_series_details(
    details: &StalkerRawSeriesDetails,
    parent: &StalkerPlaylistItem,
    added_at: i64,
    used_episode_ids: &mut HashSet<u32>,
) -> Vec<StalkerPlaylistItem> {
    let mut out = Vec::new();
    let series_id_value = details.id.as_deref().and_then(|s| s.parse::<u32>().ok()).or(parent.series_id).unwrap_or(0);
    for season in &details.seasons {
        out.extend(map_stalker_season_episodes(season, series_id_value, parent, added_at, used_episode_ids));
    }
    out
}

fn map_stalker_season_episodes(
    season: &StalkerRawSeriesSeason,
    series_id_value: u32,
    parent: &StalkerPlaylistItem,
    added_at: i64,
    used_episode_ids: &mut HashSet<u32>,
) -> Vec<StalkerPlaylistItem> {
    season
        .episodes
        .iter()
        .map(|episode| map_stalker_episode(episode, season, series_id_value, parent, added_at, used_episode_ids))
        .collect()
}

fn map_stalker_episode(
    episode: &StalkerRawSeriesEpisode,
    season: &StalkerRawSeriesSeason,
    series_id_value: u32,
    parent: &StalkerPlaylistItem,
    added_at: i64,
    used_episode_ids: &mut HashSet<u32>,
) -> StalkerPlaylistItem {
    let name = episode.display_name();
    let episode_id = episode.id.as_deref().and_then(|s| s.parse::<u32>().ok());
    let season_number = episode.season_number.or(season.number).unwrap_or(0);
    let cmd = episode.cmd.clone().unwrap_or_default();
    let info = episode.info.clone();
    let container_extension = episode.container_extension.clone().filter(|s| !s.is_empty());
    let logo = info.as_ref().and_then(|i| i.movie_image.clone()).filter(|s| !s.is_empty());

    let descriptor = if cmd.is_empty() {
        None
    } else {
        // Episodes inherit the temp-link capability of the parent series. The
        // parent carries the same flags as a normal item would (`nginx_secure_link`,
        // `flussonic_tmp_link`, ...), so we route through the same helper.
        let mode = playback_mode_from_flags(StalkerTempLinkFlags::from([
            parent.nginx_secure_link,
            parent.flussonic_tmp_link,
            parent.wowza_tmp_link,
            parent.use_http_tmp_link,
        ]));
        Some(StalkerPlaybackDescriptorDto {
            primary_mode: mode,
            candidates: vec![StalkerCommandVariantDto {
                cmd: cmd.clone(),
                playback_mode: mode,
                source_key: None,
                priority: 0,
            }],
            capabilities: None,
        })
    };

    let episode_number_u32 = episode.number.unwrap_or(0);

    let episode_storage_id = collision_free_episode_storage_id(
        series_id_value,
        season_number,
        episode.id.as_deref().unwrap_or_default(),
        episode_number_u32,
        used_episode_ids,
    );

    StalkerPlaylistItem {
        stream_id: episode_storage_id,
        name: Internable::intern(name.to_string()),
        category_id: parent.category_id,
        category_name: Arc::clone(&parent.category_name),
        number: episode_number_u32,
        logo_url: logo.map(Internable::intern),
        epg_channel_id: None,
        stream_url: Internable::intern(String::new()),
        stream_kind: StalkerStreamKind::Episode,
        cmd: Internable::intern(cmd),
        container_extension: container_extension.map(Internable::intern),
        plot: extract_info_text(info.as_ref(), |i| i.plot.clone()),
        cast: extract_info_text(info.as_ref(), |i| i.cast.clone()),
        director: extract_info_text(info.as_ref(), |i| i.director.clone()),
        genre: extract_info_text(info.as_ref(), |i| i.genre.clone()),
        release_date: extract_info_text(info.as_ref(), |i| i.releasedate.clone()),
        rating: info.as_ref().and_then(|i| i.rating).unwrap_or(0.0),
        tmdb_id: info.as_ref().and_then(|i| i.tmdb_id.clone()).and_then(|s| s.parse::<i64>().ok()),
        backdrop_url: info
            .as_ref()
            .and_then(|i| i.backdrop.clone())
            .and_then(|list| list.first().cloned())
            .filter(|s| !s.is_empty())
            .map(Internable::intern),
        added_at,
        is_adult: false,
        is_series: false,
        playback_descriptor: descriptor,
        archive_available: false,
        allow_local_timeshift: false,
        allow_local_pvr: false,
        allow_remote_pvr: false,
        nginx_secure_link: false,
        flussonic_tmp_link: false,
        wowza_tmp_link: false,
        use_http_tmp_link: false,
        series_id: Some(series_id_value),
        season_id: Some(season_number),
        episode_id,
    }
}

/// Collision-safe variant of [`stable_episode_storage_id`]: when the hashed id is
/// already taken within the current snapshot batch, the key string is deterministically
/// re-salted (`<key>:<counter>`) and re-hashed until a free slot is found. The snapshot
/// is fully rebuilt on each refresh, so within-batch determinism is all that is needed.
fn collision_free_episode_storage_id(
    series_id: u32,
    season_number: u32,
    episode_id: &str,
    episode_number: u32,
    used_ids: &mut HashSet<u32>,
) -> u32 {
    let base_key = format!("{series_id}:{season_number}:{episode_id}:{episode_number}");
    let mut id = stable_episode_storage_id(series_id, season_number, episode_id, episode_number);
    let mut salt = 0_u32;
    while !used_ids.insert(id) {
        salt = salt.saturating_add(1);
        warn!("Stalker episode storage id collision for key '{base_key}' (id {id}), re-salting with counter {salt}");
        id = fnv1a_32(&format!("{base_key}:{salt}"));
    }
    id
}

/// Log a single-line summary of a Stalker download for the given input.
pub fn log_stalker_download_summary(input_name: &str, live_count: usize, vod_count: usize, series_count: usize) {
    info!("Stalker input '{input_name}' catalog: live={live_count}, vod={vod_count}, series={series_count}");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw_with_id(id: &str, name: &str) -> StalkerRawItem {
        StalkerRawItem { id: Some(id.to_string()), name: Some(name.to_string()), ..StalkerRawItem::default() }
    }

    #[test]
    fn map_live_item_fills_defaults() {
        let raw = raw_with_id("42", "Channel 42");
        let item = map_stalker_to_playlist_item(&raw, None, StalkerStreamKind::Live, 1_700_000_000);
        assert_eq!(item.stream_id, 42);
        assert_eq!(&*item.name, "Channel 42");
        assert_eq!(item.stream_kind, StalkerStreamKind::Live);
        assert_eq!(item.category_id, 0);
        assert!(item.stream_url.is_empty());
    }

    #[test]
    fn map_live_item_preserves_unmatched_tv_genre_id() {
        let raw: StalkerRawItem = serde_json::from_value(serde_json::json!({
            "id": "42",
            "name": "Channel 42",
            "tv_genre_id": 10
        }))
        .expect("raw item");

        let item = map_stalker_to_playlist_item(&raw, None, StalkerStreamKind::Live, 0);

        assert_eq!(item.category_id, 10);
        assert!(item.category_name.is_empty());
    }

    #[test]
    fn map_vod_item_uses_movie_kind() {
        let mut raw = raw_with_id("7", "Movie 7");
        raw.info = Some(StalkerRawItemInfo {
            plot: Some("A film".to_string()),
            rating: Some(8.4),
            tmdb_id: Some("27205".to_string()),
            ..Default::default()
        });
        let item = map_stalker_to_playlist_item(&raw, None, StalkerStreamKind::Movie, 0);
        assert_eq!(item.stream_kind, StalkerStreamKind::Movie);
        assert!((item.rating - 8.4).abs() < f32::EPSILON);
        assert_eq!(item.tmdb_id, Some(27205));
        assert_eq!(item.plot.as_deref().map(ToString::to_string).as_deref(), Some("A film"));
    }

    #[test]
    fn map_live_item_copies_capability_flags() {
        let mut raw = raw_with_id("1", "x");
        raw.tv_archive = Some(true);
        raw.allow_local_timeshift = Some(true);
        raw.pvr = Some(true);
        raw.nginx_secure_link = Some(true);
        raw.use_http_tmp_link = Some(true);
        let item = map_stalker_to_playlist_item(&raw, None, StalkerStreamKind::Live, 0);
        assert!(item.archive_available);
        assert!(item.allow_local_timeshift);
        assert!(item.allow_local_pvr);
        assert!(item.nginx_secure_link);
        assert!(item.use_http_tmp_link);
    }

    #[test]
    fn map_live_item_combines_boolean_capability_aliases() {
        let mut raw = raw_with_id("1", "x");
        raw.allow_pvr = Some(false);
        raw.pvr = Some(true);
        raw.pvr_shift = Some(false);
        raw.pvr_time_shift = Some(true);

        let item = map_stalker_to_playlist_item(&raw, None, StalkerStreamKind::Live, 0);

        assert!(item.allow_local_pvr);
        assert!(item.allow_remote_pvr);
    }

    #[test]
    fn series_root_records_series_id() {
        let raw =
            StalkerRawSeriesItem { id: Some("99".to_string()), name: Some("Root".to_string()), ..Default::default() };
        let item = map_stalker_series_root(&raw, None, 0);
        assert!(item.is_series);
        assert_eq!(item.series_id, Some(99));
        assert_eq!(item.stream_id, 99);
    }

    #[test]
    fn series_details_walks_seasons_and_episodes() {
        let parent = map_stalker_series_root(
            &StalkerRawSeriesItem { id: Some("1".to_string()), name: Some("Show".to_string()), ..Default::default() },
            None,
            0,
        );
        let details = StalkerRawSeriesDetails {
            id: Some("1".to_string()),
            seasons: vec![StalkerRawSeriesSeason {
                number: Some(1),
                episodes: vec![
                    StalkerRawSeriesEpisode {
                        id: Some("10".to_string()),
                        number: Some(1),
                        name: Some("Pilot".to_string()),
                        cmd: Some("ffmpeg http://stream/1".to_string()),
                        ..Default::default()
                    },
                    StalkerRawSeriesEpisode {
                        id: Some("11".to_string()),
                        number: Some(2),
                        name: Some("Episode 2".to_string()),
                        cmd: Some("ffmpeg http://stream/2".to_string()),
                        ..Default::default()
                    },
                ],
                ..Default::default()
            }],
            ..Default::default()
        };
        let episodes = map_stalker_series_details(&details, &parent, 0, &mut HashSet::new());
        assert_eq!(episodes.len(), 2);
        assert_eq!(episodes[0].episode_id, Some(10));
        assert_eq!(episodes[0].series_id, Some(1));
        assert_eq!(episodes[0].season_id, Some(1));
        assert!(episodes[0].playback_descriptor.is_some());
        assert_eq!(&*episodes[0].cmd, "ffmpeg http://stream/1");
    }

    #[test]
    fn episode_storage_ids_include_series_context() {
        let episode = StalkerRawSeriesEpisode {
            id: Some("10".to_string()),
            number: Some(1),
            name: Some("Pilot".to_string()),
            cmd: Some("encoded".to_string()),
            ..Default::default()
        };
        let season = StalkerRawSeriesSeason { number: Some(1), episodes: vec![episode.clone()], ..Default::default() };
        let first_parent = map_stalker_series_root(
            &StalkerRawSeriesItem { id: Some("1".to_string()), name: Some("First".to_string()), ..Default::default() },
            None,
            0,
        );
        let second_parent = map_stalker_series_root(
            &StalkerRawSeriesItem { id: Some("2".to_string()), name: Some("Second".to_string()), ..Default::default() },
            None,
            0,
        );
        let first = map_stalker_episode(&episode, &season, 1, &first_parent, 0, &mut HashSet::new());
        let second = map_stalker_episode(&episode, &season, 2, &second_parent, 0, &mut HashSet::new());
        assert_ne!(first.stream_id, second.stream_id);
        assert_eq!(first.episode_id, Some(10));
    }

    #[test]
    fn episode_storage_id_collisions_are_resalted_within_a_batch() {
        let mut used = HashSet::new();
        let first = collision_free_episode_storage_id(1, 1, "10", 1, &mut used);
        // Same key again within the same batch: the hash collides with itself and
        // must be re-salted deterministically to a different, free id.
        let second = collision_free_episode_storage_id(1, 1, "10", 1, &mut used);
        assert_ne!(first, second);
        assert_eq!(first, stable_episode_storage_id(1, 1, "10", 1));
        assert_eq!(second, fnv1a_32("1:1:10:1:1"));
        // Determinism: a fresh batch produces the same sequence.
        let mut used_again = HashSet::new();
        assert_eq!(collision_free_episode_storage_id(1, 1, "10", 1, &mut used_again), first);
        assert_eq!(collision_free_episode_storage_id(1, 1, "10", 1, &mut used_again), second);
    }

    #[test]
    fn episode_container_extension_does_not_fall_back_to_release_date() {
        let episode = StalkerRawSeriesEpisode {
            id: Some("10".to_string()),
            number: Some(1),
            name: Some("Pilot".to_string()),
            cmd: Some("ffmpeg http://stream/1".to_string()),
            info: Some(StalkerRawItemInfo { releasedate: Some("2021-05-03".to_string()), ..Default::default() }),
            ..Default::default()
        };
        let season = StalkerRawSeriesSeason { number: Some(1), episodes: vec![episode.clone()], ..Default::default() };
        let parent = map_stalker_series_root(
            &StalkerRawSeriesItem { id: Some("1".to_string()), name: Some("Show".to_string()), ..Default::default() },
            None,
            0,
        );
        let item = map_stalker_episode(&episode, &season, 1, &parent, 0, &mut HashSet::new());
        assert_eq!(item.container_extension, None);
        assert_eq!(item.release_date.as_deref(), Some("2021-05-03"));
    }

    #[test]
    fn playback_mode_from_flags_nginx_wins() {
        assert_eq!(
            playback_mode_from_flags(StalkerTempLinkFlags::from([true, true, true, true])),
            StalkerPlaybackMode::TempLinkNginx
        );
    }

    #[test]
    fn playback_mode_from_flags_flussonic_when_no_nginx() {
        assert_eq!(
            playback_mode_from_flags(StalkerTempLinkFlags::from([false, true, true, true])),
            StalkerPlaybackMode::TempLinkFlussonic
        );
    }

    #[test]
    fn playback_mode_from_flags_wowza_when_only_wowza() {
        assert_eq!(
            playback_mode_from_flags(StalkerTempLinkFlags::from([false, false, true, true])),
            StalkerPlaybackMode::TempLinkWowza
        );
    }

    #[test]
    fn playback_mode_from_flags_direct_when_no_temp() {
        assert_eq!(
            playback_mode_from_flags(StalkerTempLinkFlags::from([false, false, false, false])),
            StalkerPlaybackMode::DirectUrl
        );
    }

    #[test]
    fn build_descriptor_propagates_nginx_secure_link_mode() {
        let raw = StalkerRawItem {
            id: Some("1".to_string()),
            cmd: Some("ffmpeg http://x".to_string()),
            nginx_secure_link: Some(true),
            ..Default::default()
        };
        let desc = build_descriptor_from_raw(&raw).expect("descriptor");
        assert_eq!(desc.primary_mode, StalkerPlaybackMode::TempLinkNginx);
        assert_eq!(desc.candidates.len(), 1);
        assert_eq!(desc.candidates[0].playback_mode, StalkerPlaybackMode::TempLinkNginx);
    }

    #[test]
    fn build_descriptor_falls_back_to_direct_url() {
        let raw = StalkerRawItem {
            id: Some("1".to_string()),
            cmd: Some("ffmpeg http://x".to_string()),
            ..Default::default()
        };
        let desc = build_descriptor_from_raw(&raw).expect("descriptor");
        assert_eq!(desc.primary_mode, StalkerPlaybackMode::DirectUrl);
    }
}
