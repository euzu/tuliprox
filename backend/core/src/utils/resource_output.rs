use crate::utils::request::{classify_host, ResourceDestination};
use futures::{stream, StreamExt};
use shared::model::{ResourceOutputPolicy, XtreamMappingFlags, XtreamMappingOptions, XtreamPlaylistItem};
use std::borrow::Cow;
use url::Url;

/// How many destinations of one item are classified at the same time.
///
/// An item can carry a whole season list of icons, and a lookup has its own budget, so classifying one
/// host after the other would multiply that budget by the field count.
const RESOURCE_HOST_CLASSIFY_CONCURRENCY: usize = 8;

/// Host of a resource URL without parsing it, for the shape provider data actually uses.
///
/// Returns `None` for anything a plain scan could read differently than a full parse would - user
/// info, an uppercase label, a percent escape, a backslash - so those go through [`Url::parse`].
/// Callers must treat `None` as "no host found", never as "no destination": the fallback needs to run
/// for them to get an answer. A host that the fast path reads but the parser would read differently can
/// only cause a cache miss, and a miss fails closed to a proxy link.
fn plain_resource_host(resource_url: &str) -> Option<&str> {
    let rest = resource_url.strip_prefix("http://").or_else(|| resource_url.strip_prefix("https://"))?;
    let authority = rest.split(['/', '?', '#']).next()?;
    // An IPv6 literal goes through the parser: it is written with the brackets a consumer of the host
    // expects, and the parser normalizes its spelling.
    if authority.is_empty() || authority.starts_with('[') {
        return None;
    }
    // A colon separates the port, and an authority with two of them is not a plain host.
    let (host, port) = authority.split_once(':').map_or((authority, ""), |(host, port)| (host, port));
    let is_label_byte = |byte: u8| byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'-');
    if host.is_empty()
        || host.contains(':')
        || !host.bytes().all(is_label_byte)
        || !port.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    Some(host)
}

/// Host of a resource URL that a client could fetch, if it has one.
fn resource_host(resource_url: &str) -> Option<Cow<'_, str>> {
    if let Some(host) = plain_resource_host(resource_url) {
        return Some(Cow::Borrowed(host));
    }
    let url = Url::parse(resource_url).ok()?;
    matches!(url.scheme(), "http" | "https").then(|| url.host_str().map(|host| Cow::Owned(host.to_owned())))?
}

/// Where a hop of a resource fetch must be routed.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum ResourceHop {
    /// The destination can leave the local network: fetched through the client that honours the
    /// configured proxy, so the request cannot disclose the address of this host.
    ViaProxy,
    /// The destination is only reachable from here: fetched by connecting directly, because a proxy has
    /// no route to it and its address is validated while the connection is built.
    Direct,
    /// The destination is local to this host: refused, because the fetch would turn this instance into
    /// a reader for itself.
    Blocked,
}

/// Routes a hop of a resource fetch.
///
/// [`classify_output_resource_url`] answers what a client may be handed; this answers who fetches it.
/// The class of the destination decides, with one case decided by what is *not* known: a name whose
/// lookup produced no answer is neither reachable directly nor provably public. The configured proxy is
/// then the only egress that can still resolve it, so it is used when there is one - which also keeps
/// this host's address out of the request. Without a proxy nothing changes: the direct client tries and
/// fails, as it would have anyway.
pub async fn classify_resource_hop(resource_url: &str, proxy_configured: bool) -> ResourceHop {
    let Some(host) = resource_host(resource_url) else {
        return ResourceHop::Blocked;
    };
    match classify_host(&host).await {
        (ResourceDestination::Public, _) => ResourceHop::ViaProxy,
        (ResourceDestination::Blocked, _) => ResourceHop::Blocked,
        // A destination that is not provably public connects directly: that is what keeps a
        // self-hosted server on the local network reachable, and a name that resolved to a private
        // address is exactly that. Only a name that produced no answer at all is fetched through the
        // configured proxy, because the direct client cannot resolve it either.
        (ResourceDestination::Private, answered) => {
            if answered || !proxy_configured {
                ResourceHop::Direct
            } else {
                ResourceHop::ViaProxy
            }
        }
    }
}

/// Classifies a resource URL that is about to be written into player output.
///
/// This decides what a client may reach, so the URL is read with the same parser a client uses rather
/// than with the cheaper scan [`resource_host`] offers for the hint path.
pub async fn classify_output_resource_url(resource_url: &str) -> ResourceOutputPolicy {
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
    classify_resource_host(host).await
}

/// Output policy of a single destination host.
async fn classify_resource_host(host: &str) -> ResourceOutputPolicy {
    match crate::utils::request::classify_resource_destination(host).await {
        ResourceDestination::Public => ResourceOutputPolicy::Direct,
        ResourceDestination::Private => ResourceOutputPolicy::Proxy,
        ResourceDestination::Blocked => ResourceOutputPolicy::Blocked,
    }
}

/// Records the output policy of every destination this item carries, so that rendering it needs no
/// name resolution.
///
/// Xtream output is written field by field (`to_document`), which has no place to await a lookup.
/// The item's destinations are collected and classified once here; a field for a destination that is
/// not in the map fails closed to a proxy link.
pub async fn prepare_xtream_resource_hosts(item: &XtreamPlaylistItem, options: &XtreamMappingOptions) {
    if options.web_ui_request || options.flags.contains(XtreamMappingFlags::RewriteResourceUrl) {
        return;
    }
    // The map is the dedupe: a host classified by an earlier item is skipped, so a warm map costs no
    // allocation per item. Only hosts that are new to this item are collected, and the host is read
    // without parsing the URL, because this runs for every resource field of every item.
    let mut unresolved: Vec<String> = Vec::new();
    item.visit_resource_urls(|resource_url| {
        let Some(host) = resource_host(resource_url) else {
            return;
        };
        if options.resource_host_policies.contains_key(host.as_ref())
            || unresolved.iter().any(|known| known == host.as_ref())
        {
            return;
        }
        unresolved.push(host.into_owned());
    });
    let classified: Vec<_> = stream::iter(unresolved)
        .map(|host| async move {
            let policy = classify_resource_host(&host).await;
            (host, policy)
        })
        .buffered(RESOURCE_HOST_CLASSIFY_CONCURRENCY)
        .collect()
        .await;
    for (host, policy) in classified {
        options.resource_host_policies.insert(host, policy);
    }
}

#[cfg(test)]
mod tests {
    use super::{classify_output_resource_url, prepare_xtream_resource_hosts};
    use shared::{
        model::{
            PlaylistItemType, PlaylistItemTypeSet, ResourceOutputPolicy, VirtualId, XtreamCluster,
            XtreamMappingFlagsSet, XtreamMappingOptions, XtreamPlaylistItem,
        },
        utils::Internable,
    };
    use std::sync::Arc;

    #[tokio::test]
    async fn an_unanswered_name_is_routed_through_a_configured_proxy() {
        use super::{classify_resource_hop, ResourceHop};

        // `.invalid` is reserved and never resolves: the destination is neither reachable directly nor
        // provably local, so a configured proxy is the only egress that can still resolve it.
        assert_eq!(classify_resource_hop("http://unresolved.invalid/logo.png", false).await, ResourceHop::Direct);
        assert_eq!(classify_resource_hop("http://unresolved.invalid/logo.png", true).await, ResourceHop::ViaProxy);
        // A destination on a private network is reachable directly and not through a proxy, so it stays
        // direct however a proxy is configured.
        assert_eq!(classify_resource_hop("http://192.168.1.20/logo.png", true).await, ResourceHop::Direct);
        // A local destination is refused, and a public one always uses the client that honours the proxy.
        assert_eq!(classify_resource_hop("http://127.0.0.1/logo.png", true).await, ResourceHop::Blocked);
        assert_eq!(classify_resource_hop("http://8.8.8.8/logo.png", true).await, ResourceHop::ViaProxy);
        assert_eq!(classify_resource_hop("media-server://image/plex/server/item", true).await, ResourceHop::Blocked);
    }

    #[test]
    fn resource_host_reads_the_same_host_a_parser_reads() {
        use super::{plain_resource_host, resource_host};
        use url::Url;

        // Every accepted URL either agrees with the parser, or the fast path declines and the parser
        // answers. Both directions are checked, so the shortcut can never decide a different host.
        let urls = [
            "http://cdn.example.com/logo.png",
            "https://cdn.example.com:8443/logo.png",
            "http://192.168.1.20/logo.png",
            "http://[2001:db8::1]:8080/logo.png",
            "http://example.com",
            "http://example.com?x=1",
            "http://example.com#fragment",
            "http://user:pass@example.com/logo.png",
            "http://Example.COM/logo.png",
            "http://evil.example\\@trusted.example/logo.png",
            "http://ex%41mple.com/logo.png",
            "http://example.com:abc/logo.png",
            "http://a:b:80/logo.png",
            "http:///logo.png",
            "http://.",
            "/relative/path/logo.png",
            "media-server://image/plex/server/item",
            "ftp://example.com/logo.png",
            "data:image/png;base64,AAAA",
        ];
        for url in urls {
            let parsed = Url::parse(url).ok().filter(|parsed| matches!(parsed.scheme(), "http" | "https"));
            let expected = parsed.as_ref().and_then(Url::host_str);
            match plain_resource_host(url) {
                Some(host) => assert_eq!(Some(host), expected, "fast path must agree for {url}"),
                None => assert_eq!(resource_host(url).as_deref(), expected, "fallback must answer for {url}"),
            }
        }
    }

    #[tokio::test]
    async fn private_resources_require_a_proxy_even_when_public_resources_do_not() {
        assert_eq!(classify_output_resource_url("http://192.168.1.20/logo.png").await, ResourceOutputPolicy::Proxy);
        assert_eq!(classify_output_resource_url("http://8.8.8.8/logo.png").await, ResourceOutputPolicy::Direct);
        assert_eq!(classify_output_resource_url("http://127.0.0.1/logo.png").await, ResourceOutputPolicy::Blocked);
        assert_eq!(
            classify_output_resource_url("media-server://image/plex/server/item").await,
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

        prepare_xtream_resource_hosts(&item, &options).await;

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
