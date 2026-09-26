use crate::utils::request::{classify_resource_destination, DestinationCache, ResourceDestination};
use shared::model::{ResourceOutputPolicy, XtreamMappingFlags, XtreamMappingOptions, XtreamPlaylistItem};
use std::collections::HashSet;
use url::Url;

pub async fn classify_output_resource_url(resource_url: &str, destinations: &DestinationCache) -> ResourceOutputPolicy {
    if resource_url.starts_with("/api/v1/library/thumbnail/") {
        return ResourceOutputPolicy::Direct;
    }
    if resource_url.starts_with("media-server://image/") {
        return ResourceOutputPolicy::Proxy;
    }
    let Ok(url) = Url::parse(resource_url) else {
        return ResourceOutputPolicy::Blocked;
    };
    if !matches!(url.scheme(), "http" | "https") {
        return ResourceOutputPolicy::Blocked;
    }
    let Some(host) = url.host_str() else {
        return ResourceOutputPolicy::Blocked;
    };
    match classify_resource_destination(host, destinations).await {
        ResourceDestination::Public => ResourceOutputPolicy::Direct,
        ResourceDestination::Private => ResourceOutputPolicy::Proxy,
        ResourceDestination::Blocked => ResourceOutputPolicy::Blocked,
    }
}

pub async fn prepare_xtream_resource_hosts(
    item: &XtreamPlaylistItem,
    options: &XtreamMappingOptions,
    destinations: &DestinationCache,
) {
    if options.web_ui_request || options.flags.contains(XtreamMappingFlags::RewriteResourceUrl) {
        return;
    }
    let mut hosts = HashSet::new();
    item.visit_resource_urls(|resource_url| {
        if let Ok(url) = Url::parse(resource_url) {
            if matches!(url.scheme(), "http" | "https") {
                if let Some(host) = url.host_str() {
                    hosts.insert(host.to_string());
                }
            }
        }
    });
    for host in hosts {
        let policy = match classify_resource_destination(&host, destinations).await {
            ResourceDestination::Public => ResourceOutputPolicy::Direct,
            ResourceDestination::Private => ResourceOutputPolicy::Proxy,
            ResourceDestination::Blocked => ResourceOutputPolicy::Blocked,
        };
        options.resource_host_policies.insert(host, policy);
    }
}

#[cfg(test)]
mod tests {
    use super::{classify_output_resource_url, prepare_xtream_resource_hosts};
    use crate::utils::request::DestinationCache;
    use shared::{
        model::{
            PlaylistItemType, PlaylistItemTypeSet, ResourceOutputPolicy, VirtualId, XtreamCluster,
            XtreamMappingFlagsSet, XtreamMappingOptions, XtreamPlaylistItem,
        },
        utils::Internable,
    };
    use std::sync::Arc;

    #[tokio::test]
    async fn private_resources_require_a_proxy_even_when_public_resources_do_not() {
        let destinations = DestinationCache::new();
        assert_eq!(
            classify_output_resource_url("http://192.168.1.20/logo.png", &destinations).await,
            ResourceOutputPolicy::Proxy
        );
        assert_eq!(
            classify_output_resource_url("http://8.8.8.8/logo.png", &destinations).await,
            ResourceOutputPolicy::Direct
        );
        assert_eq!(
            classify_output_resource_url("http://127.0.0.1/logo.png", &destinations).await,
            ResourceOutputPolicy::Blocked
        );
        assert_eq!(
            classify_output_resource_url("media-server://image/plex/server/item", &destinations).await,
            ResourceOutputPolicy::Proxy
        );
    }

    #[tokio::test]
    async fn xtream_output_prepares_public_and_private_resource_hosts() {
        let item = XtreamPlaylistItem {
            virtual_id: VirtualId::new(41),
            provider_id: 41,
            name: "channel".intern(),
            logo: "http://192.168.1.20/logo.png".intern(),
            logo_small: "http://8.8.8.8/logo.png".intern(),
            group: "group".intern(),
            title: "channel".intern(),
            parent_code: "".intern(),
            rec: "".intern(),
            url: "http://provider.example/live/41.ts".intern(),
            epg_channel_id: None,
            xtream_cluster: XtreamCluster::Live,
            additional_properties: None,
            item_type: PlaylistItemType::Live,
            category_id: 1,
            input_name: "input".intern(),
            channel_no: 0,
            source_ordinal: 0,
            input_stream_id: "41".intern(),
            upstream_user_agent: None,
        };
        let options = XtreamMappingOptions {
            flags: XtreamMappingFlagsSet::new(),
            force_redirect: None,
            reverse_item_types: PlaylistItemTypeSet::empty(),
            resource_proxy_item_types: PlaylistItemTypeSet::from_item(PlaylistItemType::Live),
            username: "user".to_string(),
            password: "pass".to_string(),
            base_url: "https://proxy.example".to_string(),
            web_ui_request: false,
            encrypt_secret: [0; 16],
            resource_host_policies: Arc::default(),
        };

        prepare_xtream_resource_hosts(&item, &options, &DestinationCache::new()).await;

        assert_eq!(
            options.get_resource_url(XtreamCluster::Live, PlaylistItemType::Live, item.virtual_id, &item.logo, "logo",),
            "https://proxy.example/resource/live/user/pass/41/logo"
        );
        assert_eq!(
            options.get_resource_url(
                XtreamCluster::Live,
                PlaylistItemType::Live,
                item.virtual_id,
                &item.logo_small,
                "logo_small",
            ),
            "http://8.8.8.8/logo.png"
        );
    }
}
