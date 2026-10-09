use super::{PlaylistEntry, PlaylistItemType, PlaylistItemTypeSet, XtreamCluster};
use crate::{
    create_bitset,
    model::{
        xtream_const, ClusterFlags, CommonPlaylistItem, StreamProperties, UUIDType, VirtualId, XtreamInfoDocument,
    },
    utils::{
        arc_str_option_serde, arc_str_serde, concat_path, generate_runtime_playlist_uuid, seal_web_ui_resource_url,
        Internable,
    },
};
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

create_bitset!(
    u8,
    XtreamMappingFlags,
    SkipLiveDirectSource,
    SkipVideoDirectSource,
    SkipSeriesDirectSource,
    RewriteResourceUrl
);

pub struct XtreamMappingOptions {
    pub flags: XtreamMappingFlagsSet,
    pub force_redirect: Option<ClusterFlags>,
    pub reverse_item_types: PlaylistItemTypeSet,
    pub resource_proxy_item_types: PlaylistItemTypeSet,
    pub username: String,
    pub password: String,
    pub base_url: String,
    pub web_ui_request: bool,
    pub encrypt_secret: [u8; 16],
    pub resource_host_policies: Arc<DashMap<String, ResourceOutputPolicy>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResourceOutputPolicy {
    Direct,
    Proxy,
    Blocked,
}

impl XtreamMappingOptions {
    #[inline]
    pub fn is_reverse(&self, item_type: PlaylistItemType) -> bool { self.reverse_item_types.is_set(item_type) }

    pub(super) fn is_trusted_web_ui_resource_path(resource_url: &str) -> bool {
        const TRUSTED_WEB_UI_RESOURCE_PREFIXES: [&str; 1] = ["/api/v1/library/thumbnail/"];
        TRUSTED_WEB_UI_RESOURCE_PREFIXES.iter().any(|prefix| resource_url.starts_with(prefix))
    }

    pub(super) fn build_reverse_proxy_base_url(
        &self,
        xtream_cluster: XtreamCluster,
        item_type: PlaylistItemType,
        virtual_id: VirtualId,
        force_proxy: bool,
    ) -> Option<String> {
        let proxy_resource = self.resource_proxy_item_types.is_set(item_type);
        if force_proxy || (proxy_resource && self.flags.contains(XtreamMappingFlags::RewriteResourceUrl)) {
            Some(format!(
                "{}/resource/{}/{}/{}/{}",
                self.base_url,
                xtream_cluster.as_stream_type(),
                self.username,
                self.password,
                virtual_id
            ))
        } else {
            None
        }
    }

    pub(super) fn resource_output_policy(&self, resource_url: &str) -> ResourceOutputPolicy {
        if self.flags.contains(XtreamMappingFlags::RewriteResourceUrl) {
            return ResourceOutputPolicy::Proxy;
        }
        if resource_url.starts_with("media-server://image/") {
            return ResourceOutputPolicy::Proxy;
        }
        let Ok(url) = url::Url::parse(resource_url) else {
            return if resource_url.starts_with("//")
                || resource_url.starts_with("http:")
                || resource_url.starts_with("https:")
            {
                ResourceOutputPolicy::Blocked
            } else {
                ResourceOutputPolicy::Direct
            };
        };
        if !matches!(url.scheme(), "http" | "https") {
            return ResourceOutputPolicy::Blocked;
        }
        let Some(host) = url.host_str() else {
            return ResourceOutputPolicy::Blocked;
        };
        // A host missing from the prepared set still needs a proxy link; a new resource
        // field must not expose its origin before the output classifier knows the host.
        self.resource_host_policies.get(host).map_or(ResourceOutputPolicy::Proxy, |entry| *entry)
    }

    pub fn get_resource_url(
        &self,
        xtream_cluster: XtreamCluster,
        item_type: PlaylistItemType,
        virtual_id: VirtualId,
        resource_url: &str,
        resource_field: &str,
    ) -> String {
        self.rewrite_resource_url_inner(xtream_cluster, item_type, virtual_id, resource_url, |base| {
            format!("{base}/{resource_field}")
        })
    }

    pub fn get_bd_path_resource_url(
        &self,
        xtream_cluster: XtreamCluster,
        item_type: PlaylistItemType,
        virtual_id: VirtualId,
        resource_url: &str,
        resource_field: &str,
        index: usize,
    ) -> String {
        self.rewrite_resource_url_inner(xtream_cluster, item_type, virtual_id, resource_url, |base| {
            format!("{base}/{resource_field}{}_{index}", xtream_const::XC_PROP_BACKDROP_PATH)
        })
    }

    pub(super) fn rewrite_resource_url_inner(
        &self,
        xtream_cluster: XtreamCluster,
        item_type: PlaylistItemType,
        virtual_id: VirtualId,
        resource_url: &str,
        proxy_suffix: impl FnOnce(&str) -> String,
    ) -> String {
        let Some(resource_url) = super::super::persisted_resource_url(resource_url) else {
            return String::new();
        };
        let resource_url = resource_url.as_ref();
        if !self.web_ui_request && Self::is_trusted_web_ui_resource_path(resource_url) {
            return concat_path(&self.base_url, resource_url);
        }

        if self.web_ui_request {
            if resource_url.is_empty() {
                return resource_url.to_string();
            }
            if Self::is_trusted_web_ui_resource_path(resource_url) {
                return resource_url.to_string();
            }
            return concat_path(&self.base_url, &seal_web_ui_resource_url(&self.encrypt_secret, resource_url));
        }

        let policy = self.resource_output_policy(resource_url);
        if policy == ResourceOutputPolicy::Blocked {
            return String::new();
        }
        let rewrite_url = self.build_reverse_proxy_base_url(
            xtream_cluster,
            item_type,
            virtual_id,
            policy == ResourceOutputPolicy::Proxy,
        );

        if let Some(url) = rewrite_url {
            if resource_url.starts_with("http://")
                || resource_url.starts_with("https://")
                || resource_url.starts_with("media-server://image/")
            {
                return proxy_suffix(&url);
            }
        }
        resource_url.to_string()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct XtreamPlaylistItem {
    pub virtual_id: VirtualId,
    pub provider_id: u32,
    #[serde(with = "arc_str_serde")]
    pub name: Arc<str>,
    #[serde(with = "arc_str_serde")]
    pub logo: Arc<str>,
    #[serde(with = "arc_str_serde")]
    pub logo_small: Arc<str>,
    #[serde(with = "arc_str_serde")]
    pub group: Arc<str>,
    #[serde(with = "arc_str_serde")]
    pub title: Arc<str>,
    #[serde(with = "arc_str_serde")]
    pub parent_code: Arc<str>,
    #[serde(with = "arc_str_serde")]
    pub rec: Arc<str>,
    #[serde(with = "arc_str_serde")]
    pub url: Arc<str>,
    #[serde(default, with = "arc_str_option_serde")]
    pub epg_channel_id: Option<Arc<str>>,
    pub xtream_cluster: XtreamCluster,
    #[serde(default)]
    pub additional_properties: Option<StreamProperties>,
    pub item_type: PlaylistItemType,
    pub category_id: u32,
    #[serde(with = "arc_str_serde")]
    pub input_name: Arc<str>,
    pub channel_no: u32,
    #[serde(default)]
    pub source_ordinal: u32,
    /// Stable provider/origin ID captured before any target transformation.
    #[serde(default, with = "arc_str_serde")]
    pub input_stream_id: Arc<str>,
    #[serde(default, rename = "source_user_agent", alias = "upstream_user_agent", with = "arc_str_option_serde")]
    pub upstream_user_agent: Option<Arc<str>>,
}

impl XtreamPlaylistItem {
    pub fn to_common(&self) -> CommonPlaylistItem {
        CommonPlaylistItem {
            virtual_id: self.virtual_id,
            provider_id: self.provider_id.intern(),
            name: self.name.clone(),
            chno: self.channel_no,
            logo: self.logo.clone(),
            logo_small: self.logo_small.clone(),
            group: self.group.clone(),
            title: self.title.clone(),
            parent_code: self.parent_code.clone(),
            audio_track: "".intern(),
            time_shift: "".intern(),
            rec: self.rec.clone(),
            url: self.url.clone(),
            input_name: self.input_name.clone(),
            item_type: self.item_type,
            epg_channel_id: self.epg_channel_id.clone(),
            xtream_cluster: Some(self.xtream_cluster),
            additional_properties: self.additional_properties.clone(),
            category_id: Some(self.category_id),
        }
    }

    pub fn get_container_extension(&self) -> Option<Arc<str>> {
        match self.additional_properties {
            None => None,
            Some(ref props) => match props {
                StreamProperties::Live(_) => Some("ts".intern()),
                StreamProperties::Video(video) => Some(Arc::clone(&video.container_extension)),
                StreamProperties::Series(_) => None,
                StreamProperties::Episode(episode) => Some(Arc::clone(&episode.container_extension)),
            },
        }
    }

    #[inline]
    pub fn has_details(&self) -> bool {
        self.additional_properties.as_ref().is_some_and(super::super::stream_properties::StreamProperties::has_details)
    }

    pub fn visit_resource_urls(&self, mut visit: impl FnMut(&str)) {
        let mut visit_persisted = |value: &str| {
            if let Some(url) = super::super::persisted_resource_url(value) {
                visit(url.as_ref());
            }
        };
        visit_persisted(&self.logo);
        visit_persisted(&self.logo_small);
        match self.additional_properties.as_ref() {
            Some(StreamProperties::Live(live)) => visit_persisted(&live.stream_icon),
            Some(StreamProperties::Video(video)) => {
                visit_persisted(&video.stream_icon);
                if let Some(details) = video.details.as_ref() {
                    if let Some(url) = details.cover_big.as_deref() {
                        visit_persisted(url);
                    }
                    if let Some(url) = details.movie_image.as_deref() {
                        visit_persisted(url);
                    }
                    if let Some(backdrops) = details.backdrop_path.as_ref() {
                        for url in backdrops {
                            visit_persisted(url);
                        }
                    }
                }
            }
            Some(StreamProperties::Series(series)) => {
                visit_persisted(&series.cover);
                if let Some(backdrops) = series.backdrop_path.as_ref() {
                    for url in backdrops {
                        visit_persisted(url);
                    }
                }
                if let Some(details) = series.details.as_ref() {
                    if let Some(seasons) = details.seasons.as_ref() {
                        for season in seasons {
                            for url in [&season.cover, &season.cover_tmdb, &season.cover_big].into_iter().flatten() {
                                visit_persisted(url);
                            }
                        }
                    }
                    if let Some(episodes) = details.episodes.as_ref() {
                        for episode in episodes {
                            visit_persisted(&episode.movie_image);
                        }
                    }
                }
            }
            Some(StreamProperties::Episode(episode)) => visit_persisted(&episode.movie_image),
            None => {}
        }
    }

    pub fn resolve_resource_url(&self, field: &str) -> Option<Arc<str>> {
        let bytes = field.as_bytes();
        let url = if bytes.eq_ignore_ascii_case(b"logo") && !self.logo.is_empty() {
            Some(Arc::clone(&self.logo))
        } else if bytes.eq_ignore_ascii_case(b"logo_small") && !self.logo_small.is_empty() {
            Some(Arc::clone(&self.logo_small))
        } else {
            self.additional_properties.as_ref().and_then(|a| a.resolve_resource_url(field))
        }?;
        super::super::persisted_resource_url(&url).map(|decoded| match decoded {
            std::borrow::Cow::Borrowed(_) => Arc::clone(&url),
            std::borrow::Cow::Owned(value) => Arc::from(value),
        })
    }
}

impl PlaylistEntry for XtreamPlaylistItem {
    #[inline]
    fn get_virtual_id(&self) -> VirtualId { self.virtual_id }
    fn get_input_stream_id(&self) -> Option<Arc<str>> {
        if self.input_stream_id.is_empty() {
            (self.provider_id > 0).then(|| self.provider_id.to_string().intern())
        } else {
            Some(Arc::clone(&self.input_stream_id))
        }
    }

    fn get_upstream_user_agent(&self) -> Option<&str> { self.upstream_user_agent.as_deref() }
    #[inline]
    fn get_provider_id(&self) -> Option<u32> { Some(self.provider_id) }
    #[inline]
    fn get_category_id(&self) -> Option<u32> { Some(self.category_id) }
    #[inline]
    fn get_provider_url(&self) -> Arc<str> { Arc::clone(&self.url) }

    #[inline]
    fn get_uuid(&self) -> UUIDType {
        generate_runtime_playlist_uuid(&self.input_name, &self.provider_id.to_string(), self.item_type, &self.url)
    }
    #[inline]
    fn get_item_type(&self) -> PlaylistItemType { self.item_type }
    #[inline]
    fn get_group(&self) -> Arc<str> { Arc::clone(&self.group) }
    #[inline]
    fn get_name(&self) -> Arc<str> {
        if self.title.is_empty() {
            Arc::clone(&self.name)
        } else {
            Arc::clone(&self.title)
        }
    }

    fn get_resolved_info_document(&self, options: &XtreamMappingOptions) -> Option<XtreamInfoDocument> {
        if self.has_details() {
            self.additional_properties.as_ref().map(|p| {
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

    #[inline]
    fn get_additional_properties(&self) -> Option<&StreamProperties> { self.additional_properties.as_ref() }
    #[inline]
    fn get_additional_properties_mut(&mut self) -> Option<&mut StreamProperties> { self.additional_properties.as_mut() }
}

impl From<XtreamPlaylistItem> for CommonPlaylistItem {
    fn from(item: XtreamPlaylistItem) -> Self { item.to_common() }
}
