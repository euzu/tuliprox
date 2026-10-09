use super::*;
use crate::{
    model::{
        stalker::StalkerStreamKind, stalker_item::StalkerPlaylistItem, CatchupAttribute, CatchupProperties,
        HeaderField, LiveStreamProperties, PlaylistItemType, StreamProperties, VirtualId, XtreamCluster,
        XtreamMappingFlags,
    },
    utils::{concat_path, seal_web_ui_resource_url, Internable},
};
use std::sync::Arc;

fn sample_options() -> XtreamMappingOptions {
    XtreamMappingOptions {
        base_url: "/api/v1/playlist/resource".to_string(),
        username: "user".to_string(),
        password: "pass".to_string(),
        force_redirect: None,
        reverse_item_types: PlaylistItemTypeSet::from_item(PlaylistItemType::Live),
        resource_proxy_item_types: PlaylistItemTypeSet::from_item(PlaylistItemType::Live),
        web_ui_request: true,
        flags: XtreamMappingFlags::RewriteResourceUrl.into(),
        encrypt_secret: [3u8; 16],
        resource_host_policies: Arc::default(),
    }
}

mod groups;
mod identity;
mod m3u;
mod xtream;
