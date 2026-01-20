use crate::utils::arc_str_serde;
use crate::utils::is_blank_optional_string;
use std::net::SocketAddr;
use std::sync::Arc;
use serde::{Deserialize, Serialize};
use crate::model::{M3uPlaylistItem, PlaylistEntry, PlaylistItemType, XtreamCluster, XtreamPlaylistItem};
use crate::utils::{current_time_secs, longest};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StreamChannel {
    pub target_id: u16,
    pub virtual_id: u32,
    pub provider_id: u32,
    pub item_type: PlaylistItemType,
    pub cluster: XtreamCluster,
    #[serde(with = "arc_str_serde")]
    pub group: Arc<str>,
    #[serde(with = "arc_str_serde")]
    pub title: Arc<str>,
    #[serde(with = "arc_str_serde")]
    pub url: Arc<str>,
    pub shared: bool,
}

pub fn create_stream_channel_with_type(target_id: u16, pli: &XtreamPlaylistItem, item_type: PlaylistItemType) -> StreamChannel {
    let mut stream_channel = pli.to_stream_channel(target_id);
    stream_channel.item_type = item_type;
    stream_channel
}

impl XtreamPlaylistItem {
    pub fn to_stream_channel(&self, target_id: u16) -> StreamChannel {
        StreamChannel {
            target_id,
            virtual_id: self.virtual_id,
            provider_id: self.provider_id,
            item_type: self.item_type,
            cluster: self.xtream_cluster,
            group: Arc::clone(&self.group),
            title: Arc::clone(longest(&self.title, &self.name)),
            url: Arc::clone(&self.url),
            shared: false,
        }
    }
}

impl M3uPlaylistItem {
    pub fn to_stream_channel(&self, target_id: u16) -> StreamChannel {
        StreamChannel {
            target_id,
            virtual_id: self.virtual_id,
            provider_id: self.get_provider_id().unwrap_or_default(),
            item_type: self.item_type,
            cluster: XtreamCluster::try_from(self.item_type).unwrap_or(XtreamCluster::Live),
            group: Arc::clone(&self.group),
            title: Arc::clone(longest(&self.title, &self.name)),
            url: Arc::clone(&self.url),
            shared: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StreamInfo {
    pub username: String,
    pub channel: StreamChannel,
    pub provider: String,
    pub addr: SocketAddr,
    pub client_ip: String,
    #[serde(default)]
    pub user_agent: String,
    #[serde(default)]
    pub ts: u64,
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub country: Option<String>,
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub session_token: Option<String>,
}

impl StreamInfo {
    #[allow(clippy::too_many_arguments)]
    pub fn new(username: &str, addr: &SocketAddr, client_ip: &str, provider: &str,
               stream_channel: StreamChannel, user_agent: String, country: Option<String>,
               session_token: Option<&str>) -> Self {
        Self {
            username: username.to_string(),
            channel: stream_channel,
            provider: provider.to_string(),
            addr: *addr,
            client_ip: client_ip.to_string(),
            user_agent,
            ts: current_time_secs(),
            country,
            session_token: session_token.map(|token| token.to_string()),
        }
    }
}
