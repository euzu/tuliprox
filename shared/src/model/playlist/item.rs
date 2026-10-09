use super::{
    M3uPlaylistItem, PlaylistItemHeader, PlaylistItemType, XtreamCluster, XtreamMappingOptions, XtreamPlaylistItem,
};
use crate::{
    model::{
        stalker::StalkerStreamKind, stalker_item::StalkerPlaylistItem, CommonPlaylistItem, EpisodeStreamProperties,
        SeriesStreamProperties, StreamProperties, UUIDType, VideoStreamProperties, VirtualId, XtreamInfoDocument,
    },
    utils::{
        extract_extension_from_url, generate_provider_playlist_uuid, generate_runtime_playlist_uuid, get_provider_id,
        Internable,
    },
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

pub trait PlaylistEntry: Send + Sync {
    fn get_virtual_id(&self) -> VirtualId;
    /// Returns the immutable stream identifier captured from the input playlist before target mappings.
    fn get_input_stream_id(&self) -> Option<Arc<str>>;
    fn get_provider_id(&self) -> Option<u32>;
    fn get_category_id(&self) -> Option<u32>;
    fn get_provider_url(&self) -> Arc<str>;
    fn get_uuid(&self) -> UUIDType;
    fn get_item_type(&self) -> PlaylistItemType;
    fn get_group(&self) -> Arc<str>;
    fn get_name(&self) -> Arc<str>;
    fn get_resolved_info_document(&self, options: &XtreamMappingOptions) -> Option<XtreamInfoDocument>;
    fn get_additional_properties(&self) -> Option<&StreamProperties>;
    fn get_additional_properties_mut(&mut self) -> Option<&mut StreamProperties>;
    fn get_upstream_user_agent(&self) -> Option<&str>;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlaylistItem {
    #[serde(flatten)]
    pub header: PlaylistItemHeader,
}

impl PlaylistItem {
    pub(super) fn get_additional_properties(header: &PlaylistItemHeader) -> Option<StreamProperties> {
        match &header.additional_properties {
            Some(props) => Some(props.clone()),
            None => {
                match header.xtream_cluster {
                    XtreamCluster::Live => None,
                    XtreamCluster::Video => {
                        let container_extension = extract_extension_from_url(&header.url)
                            .map(|e| e.strip_prefix('.').unwrap_or(e).to_string())
                            .unwrap_or_default();
                        Some(StreamProperties::Video(Box::new(VideoStreamProperties {
                            name: header.name.clone(),
                            category_id: header.category_id,
                            stream_id: header.virtual_id.get(),
                            stream_icon: "".intern(),
                            direct_source: "".intern(),
                            custom_sid: None,
                            added: "".intern(),
                            container_extension: container_extension.intern(),
                            rating: None,
                            rating_5based: None,
                            stream_type: Some("movie".intern()),
                            trailer: None,
                            tmdb: None,
                            is_adult: 0,
                            details: None,
                        })))
                    }
                    XtreamCluster::Series => {
                        if header.item_type == PlaylistItemType::Series {
                            let container_extension = extract_extension_from_url(&header.url)
                                .map(|e| e.strip_prefix('.').unwrap_or(e).to_string())
                                .unwrap_or_default();
                            // TODO maybe from link ? like s01e02 or something like this
                            Some(StreamProperties::Episode(Box::new(EpisodeStreamProperties {
                                episode_id: 0,
                                episode: 0,
                                season: 0,
                                added: None,
                                release_date: None,
                                series_release_date: None,
                                plot: None,
                                tmdb: None,
                                movie_image: "".intern(),
                                container_extension: container_extension.intern(),
                                audio: None,
                                video: None,
                            })))
                        } else if header.item_type == PlaylistItemType::SeriesInfo {
                            Some(StreamProperties::Series(Box::new(SeriesStreamProperties {
                                name: header.name.clone(),
                                category_id: header.category_id,
                                tmdb: None,
                                series_id: 0,
                                backdrop_path: None,
                                cast: "".intern(),
                                cover: "".intern(),
                                director: "".intern(),
                                episode_run_time: None,
                                genre: None,
                                last_modified: None,
                                plot: None,
                                rating: 0.0,
                                rating_5based: 0.0,
                                release_date: None,
                                youtube_trailer: "".intern(),
                                details: None,
                            })))
                        } else {
                            None
                        }
                    }
                }
            }
        }
    }

    pub fn has_details(&self) -> bool {
        self.header
            .additional_properties
            .as_ref()
            .is_some_and(super::super::stream_properties::StreamProperties::has_details)
    }

    pub fn get_tmdb_id(&self) -> Option<u32> {
        self.header
            .additional_properties
            .as_ref()
            .and_then(super::super::stream_properties::StreamProperties::get_tmdb_id)
    }
}

impl From<&PlaylistItem> for XtreamPlaylistItem {
    fn from(item: &PlaylistItem) -> Self {
        let header = &item.header;
        let input_stream_id = Arc::clone(&header.input_stream_id);
        let missing_live_input_identity =
            input_stream_id.is_empty() && (header.item_type.is_live() || header.item_type == PlaylistItemType::Catchup);
        let provider_id =
            if missing_live_input_identity { 0 } else { get_provider_id(&header.id, &header.url).unwrap_or_default() };
        let additional_properties = PlaylistItem::get_additional_properties(header);

        XtreamPlaylistItem {
            virtual_id: header.virtual_id,
            provider_id,
            name: if header.item_type == PlaylistItemType::Series {
                Arc::clone(&header.title)
            } else {
                Arc::clone(&header.name)
            },
            logo: Arc::clone(&header.logo),
            logo_small: Arc::clone(&header.logo_small),
            group: Arc::clone(&header.group),
            title: Arc::clone(&header.title),
            parent_code: Arc::clone(&header.parent_code),
            rec: Arc::clone(&header.rec),
            url: Arc::clone(&header.url),
            epg_channel_id: header.epg_channel_id.clone(),
            xtream_cluster: header.xtream_cluster,
            additional_properties,
            item_type: header.item_type,
            category_id: header.category_id,
            input_name: Arc::clone(&header.input_name),
            channel_no: header.chno,
            source_ordinal: header.source_ordinal,
            input_stream_id,
            upstream_user_agent: header.upstream_user_agent.clone(),
        }
    }
}

impl From<&PlaylistItem> for M3uPlaylistItem {
    fn from(item: &PlaylistItem) -> Self {
        let header = &item.header;
        let input_stream_id = Arc::clone(&header.input_stream_id);
        let missing_live_input_identity =
            input_stream_id.is_empty() && (header.item_type.is_live() || header.item_type == PlaylistItemType::Catchup);
        M3uPlaylistItem {
            virtual_id: header.virtual_id,
            provider_id: if missing_live_input_identity { "".intern() } else { Arc::clone(&header.id) },
            name: if header.item_type == PlaylistItemType::Series {
                Arc::clone(&header.title)
            } else {
                Arc::clone(&header.name)
            },
            chno: header.chno,
            logo: Arc::clone(&header.logo),
            logo_small: Arc::clone(&header.logo_small),
            group: Arc::clone(&header.group),
            title: Arc::clone(&header.title),
            parent_code: Arc::clone(&header.parent_code),
            audio_track: Arc::clone(&header.audio_track),
            time_shift: Arc::clone(&header.time_shift),
            rec: Arc::clone(&header.rec),
            url: Arc::clone(&header.url),
            epg_channel_id: header.epg_channel_id.clone(),
            input_name: Arc::clone(&header.input_name),
            item_type: header.item_type,
            t_stream_url: Arc::clone(&header.url),
            t_resource_url: None,
            t_catchup_source: None,
            t_catchup_mode: None,
            source_ordinal: header.source_ordinal,
            additional_properties: header.additional_properties.clone(),
            input_stream_id,
            upstream_user_agent: header.upstream_user_agent.clone(),
        }
    }
}

impl From<&PlaylistItem> for CommonPlaylistItem {
    fn from(item: &PlaylistItem) -> Self {
        let header = &item.header;

        let additional_properties = PlaylistItem::get_additional_properties(header);

        CommonPlaylistItem {
            virtual_id: header.virtual_id,
            provider_id: Arc::clone(&header.id),
            name: if header.item_type == PlaylistItemType::Series {
                Arc::clone(&header.title)
            } else {
                Arc::clone(&header.name)
            },
            logo: header.logo.clone(),
            logo_small: header.logo_small.clone(),
            group: Arc::clone(&header.group),
            title: header.title.clone(),
            parent_code: header.parent_code.clone(),
            audio_track: header.audio_track.clone(),
            time_shift: header.time_shift.clone(),
            rec: header.rec.clone(),
            url: header.url.clone(),
            epg_channel_id: header.epg_channel_id.clone(),
            xtream_cluster: Some(header.xtream_cluster),
            additional_properties,
            item_type: header.item_type,
            category_id: Some(header.category_id),
            input_name: Arc::clone(&header.input_name),
            chno: header.chno,
        }
    }
}

impl From<&XtreamPlaylistItem> for PlaylistItem {
    fn from(item: &XtreamPlaylistItem) -> Self {
        let input_stream_id = item.get_input_stream_id();
        let header = PlaylistItemHeader {
            uuid: item.get_uuid(),
            virtual_id: item.virtual_id,
            id: if item.provider_id == 0 && input_stream_id.is_none() {
                "".intern()
            } else {
                item.provider_id.to_string().intern()
            },
            name: item.name.clone(),
            title: item.title.clone(),
            logo: item.logo.clone(),
            logo_small: item.logo_small.clone(),
            group: item.group.clone(),
            parent_code: item.parent_code.clone(),
            rec: item.rec.clone(),
            url: item.url.clone(),
            epg_channel_id: item.epg_channel_id.clone(),
            xtream_cluster: item.xtream_cluster,
            item_type: item.item_type,
            category_id: item.category_id,
            input_name: item.input_name.clone(),
            chno: item.channel_no,
            audio_track: "".intern(),
            time_shift: "".intern(),
            additional_properties: item.additional_properties.clone(),
            source_ordinal: item.source_ordinal,
            input_stream_id: input_stream_id.unwrap_or_else(|| "".intern()),
            upstream_user_agent: item.upstream_user_agent.clone(),
        };

        PlaylistItem { header }
    }
}

impl From<&M3uPlaylistItem> for PlaylistItem {
    fn from(item: &M3uPlaylistItem) -> Self {
        let header = PlaylistItemHeader {
            uuid: item.get_uuid(),
            virtual_id: item.virtual_id,
            id: item.provider_id.clone(),
            name: item.name.clone(),
            title: item.title.clone(),
            logo: item.logo.clone(),
            logo_small: item.logo_small.clone(),
            group: item.group.clone(),
            parent_code: item.parent_code.clone(),
            rec: item.rec.clone(),
            url: item.url.clone(),
            epg_channel_id: item.epg_channel_id.clone(),
            xtream_cluster: item.item_type.cluster(),
            item_type: item.item_type,
            category_id: 0,
            input_name: item.input_name.clone(),
            chno: item.chno,
            audio_track: item.audio_track.clone(),
            time_shift: item.time_shift.clone(),
            additional_properties: item.additional_properties.clone(),
            source_ordinal: item.source_ordinal,
            input_stream_id: item.get_input_stream_id().unwrap_or_else(|| "".intern()),
            upstream_user_agent: item.upstream_user_agent.clone(),
        };

        PlaylistItem { header }
    }
}

impl PlaylistItem {
    /// Canonical `StalkerPlaylistItem` → `PlaylistItem` conversion.
    ///
    /// This is the single conversion used by both the download path
    /// (processor) and the disk-load path so the generated identity is stable
    /// across processing modes: the UUID is always seeded with the owning
    /// input name and the `input_name` header field is always populated.
    /// The group falls back to a cluster default only when the portal did
    /// not supply a category name.
    pub fn from_stalker(item: &StalkerPlaylistItem, input_name: &str) -> Self {
        let item_type = match item.stream_kind {
            StalkerStreamKind::Live | StalkerStreamKind::Archive => PlaylistItemType::Live,
            StalkerStreamKind::Movie => PlaylistItemType::Video,
            StalkerStreamKind::Episode => {
                if item.is_series_root() {
                    PlaylistItemType::SeriesInfo
                } else {
                    PlaylistItemType::Series
                }
            }
        };
        let xtream_cluster = match item.stream_kind {
            StalkerStreamKind::Live | StalkerStreamKind::Archive => XtreamCluster::Live,
            StalkerStreamKind::Movie => XtreamCluster::Video,
            StalkerStreamKind::Episode => XtreamCluster::Series,
        };
        let stream_id_str: Arc<str> = Internable::intern(item.stream_id.to_string());
        let logo: Arc<str> = item.logo_url.clone().unwrap_or_else(|| Internable::intern(String::new()));
        // Keep unresolved commands private; playback resolves them from the persisted Stalker metadata.
        let url = Arc::clone(&item.stream_url);
        let group: Arc<str> = if item.category_name.is_empty() {
            let fallback = match item.stream_kind {
                StalkerStreamKind::Live | StalkerStreamKind::Archive => "Live",
                StalkerStreamKind::Movie => "Movies",
                StalkerStreamKind::Episode => "Series",
            };
            Internable::intern(fallback.to_string())
        } else {
            Arc::clone(&item.category_name)
        };
        let header = PlaylistItemHeader {
            uuid: generate_provider_playlist_uuid(input_name, &stream_id_str, item_type),
            virtual_id: VirtualId::new(item.stream_id),
            id: Arc::clone(&stream_id_str),
            name: Arc::clone(&item.name),
            title: Arc::clone(&item.name),
            logo: Arc::clone(&logo),
            logo_small: Internable::intern(String::new()),
            group,
            parent_code: Internable::intern(String::new()),
            audio_track: Internable::intern(String::new()),
            time_shift: Internable::intern(String::new()),
            rec: Internable::intern(String::new()),
            url,
            epg_channel_id: item.epg_channel_id.clone(),
            item_type,
            xtream_cluster,
            additional_properties: None,
            input_name: Internable::intern(input_name.to_string()),
            chno: item.number,
            category_id: item.category_id,
            source_ordinal: 0,
            input_stream_id: stream_id_str,
            upstream_user_agent: None,
        };

        PlaylistItem { header }
    }
}

impl PlaylistEntry for PlaylistItem {
    #[inline]
    fn get_virtual_id(&self) -> VirtualId { self.header.virtual_id }

    #[inline]
    fn get_input_stream_id(&self) -> Option<Arc<str>> { self.header.get_input_stream_id() }

    fn get_upstream_user_agent(&self) -> Option<&str> { self.header.upstream_user_agent.as_deref() }

    fn get_provider_id(&self) -> Option<u32> {
        let header = &self.header;
        get_provider_id(&header.id, &header.url)
    }

    #[inline]
    fn get_category_id(&self) -> Option<u32> { Some(self.header.category_id) }

    #[inline]
    fn get_provider_url(&self) -> Arc<str> { Arc::clone(&self.header.url) }

    #[inline]
    fn get_uuid(&self) -> UUIDType {
        let header = &self.header;
        generate_runtime_playlist_uuid(&header.input_name, &header.id, header.item_type, &header.url)
    }

    #[inline]
    fn get_item_type(&self) -> PlaylistItemType { self.header.item_type }

    #[inline]
    fn get_group(&self) -> Arc<str> { Arc::clone(&self.header.group) }

    #[inline]
    fn get_name(&self) -> Arc<str> { self.header.get_name() }

    fn get_resolved_info_document(&self, options: &XtreamMappingOptions) -> Option<XtreamInfoDocument> {
        if self.has_details() {
            self.header.additional_properties.as_ref().map(|p| {
                p.to_info_document(
                    options,
                    self.get_item_type(),
                    self.get_virtual_id(),
                    self.get_category_id().unwrap_or(0),
                )
            })
        } else {
            None
        }
    }

    fn get_additional_properties(&self) -> Option<&StreamProperties> { self.header.additional_properties.as_ref() }
    #[inline]
    fn get_additional_properties_mut(&mut self) -> Option<&mut StreamProperties> {
        self.header.additional_properties.as_mut()
    }
}
