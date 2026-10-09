use super::{
    PublicIpResolver, ResourceDestination, ResourceDestinationResolver, RESOURCE_DESTINATION_CACHE_CAPACITY,
    RESOURCE_DESTINATION_LOOKUP_TIMEOUT, RESOURCE_DESTINATION_PRIVATE_TTL, RESOURCE_DESTINATION_PUBLIC_TTL,
    RESOURCE_DESTINATION_UNANSWERED_TTL,
};
use crate::model::AppConfig;
use lru::LruCache;
use std::{
    io::{Error, ErrorKind},
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    num::NonZeroUsize,
    sync::{Arc, Mutex, OnceLock, PoisonError},
    time::{Duration, Instant},
};
use tokio::time::timeout;

impl reqwest::dns::Resolve for PublicIpResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let host = name.as_str().to_string();
        Box::pin(async move {
            let addresses = resolve_public_socket_addrs(&host, 0)
                .await
                .map_err(|err| Box::new(err) as Box<dyn std::error::Error + Send + Sync>)?;
            Ok(Box::new(addresses.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

pub async fn resolve_public_socket_addrs(host: &str, port: u16) -> std::io::Result<Vec<SocketAddr>> {
    let addresses = lookup_socket_addrs(host, port).await?;
    if addresses.is_empty() || addresses.iter().any(|address| !is_public_ip(address.ip())) {
        return Err(Error::new(ErrorKind::PermissionDenied, "destination resolves to a non-public address"));
    }
    Ok(addresses)
}

/// Resolves a resource destination, refusing addresses that are local to this host.
///
/// Private network destinations stay allowed: self-hosted services are a normal resource source.
pub async fn resolve_resource_socket_addrs(host: &str, port: u16) -> std::io::Result<Vec<SocketAddr>> {
    let addresses = lookup_socket_addrs(host, port).await?;
    if addresses.is_empty() {
        return Err(Error::new(ErrorKind::PermissionDenied, "destination does not resolve"));
    }
    if addresses.iter().any(|address| classify_ip(address.ip()) == ResourceDestination::Blocked) {
        return Err(Error::new(ErrorKind::PermissionDenied, "destination is local to this host"));
    }
    Ok(addresses)
}

/// Resolves a host name or IP literal, bracketed IPv6 included. An unresolvable name yields an empty
/// result so that every policy decides on its own how to report it.
async fn lookup_socket_addrs(host: &str, port: u16) -> std::io::Result<Vec<SocketAddr>> {
    if let Ok(address) = host_literal(host).parse::<IpAddr>() {
        return Ok(vec![SocketAddr::new(address, port)]);
    }
    Ok(tokio::net::lookup_host((host, port)).await?.collect())
}

/// Hosts of the proxies an outbound request may be routed through.
///
/// A resource client that honours the configured proxy asks its resolver about the proxy host as well,
/// and a proxy commonly lives on the loopback interface (`http://localhost:8118`). That name is exempt
/// from the local-address guard, because a proxy on this host is a valid destination for this host.
pub(super) fn proxy_hosts(app_config: &AppConfig) -> Vec<Arc<str>> {
    const ENV_KEYS: [&str; 3] = ["HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY"];
    let mut hosts = Vec::new();
    let config = app_config.config.load();
    if let Some(proxy) = config.proxy.as_ref() {
        if let Some(host) =
            crate::model::parse_proxy_url_with_http_fallback(&proxy.url).and_then(|url| url.host_str().map(Arc::from))
        {
            hosts.push(host);
        }
    }
    drop(config);
    for (key, value) in std::env::vars_os() {
        let (Some(key), Some(value)) = (key.to_str(), value.to_str()) else {
            continue;
        };
        if !ENV_KEYS.iter().any(|candidate| candidate.eq_ignore_ascii_case(key)) {
            continue;
        }
        if let Some(host) =
            crate::model::parse_proxy_url_with_http_fallback(value).and_then(|url| url.host_str().map(Arc::from))
        {
            hosts.push(host);
        }
    }
    hosts
}

impl ResourceDestinationResolver {
    /// Resolver that resolves the proxy hosts of this configuration and guards every other name.
    pub fn allowing_proxy_hosts(app_config: &AppConfig) -> Self {
        Self { allowed_hosts: proxy_hosts(app_config).into() }
    }
}

impl reqwest::dns::Resolve for ResourceDestinationResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let host = name.as_str().to_owned();
        let allowed = self.allowed_hosts.iter().any(|allowed| allowed.as_ref().eq_ignore_ascii_case(&host));
        Box::pin(async move {
            let addresses = if allowed {
                lookup_socket_addrs(&host, 0).await
            } else {
                resolve_resource_socket_addrs(&host, 0).await
            }
            .map_err(|err| Box::new(err) as Box<dyn std::error::Error + Send + Sync>)?;
            Ok(Box::new(addresses.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

/// Reachability class of an address. Single place that decides which range belongs to which class.
pub fn classify_ip(address: IpAddr) -> ResourceDestination {
    match address {
        IpAddr::V4(address) => classify_ipv4(address),
        IpAddr::V6(address) => classify_ipv6(address),
    }
}

fn classify_ipv4(address: Ipv4Addr) -> ResourceDestination {
    let [first, second, _, _] = address.octets();
    if address.is_loopback()
        || address.is_link_local()
        || address.is_unspecified()
        || address.is_multicast()
        || address.is_broadcast()
        || first == 0
    {
        ResourceDestination::Blocked
    } else if address.is_private()
        || address.is_documentation()
        || (first == 100 && (64..=127).contains(&second))
        || (first == 198 && matches!(second, 18 | 19))
        || first >= 240
    {
        ResourceDestination::Private
    } else {
        ResourceDestination::Public
    }
}

/// IPv4 address carried inside an IPv6 address: IPv4-compatible, NAT64 (`64:ff9b::/96`) or 6to4.
fn embedded_ipv4(address: Ipv6Addr) -> Option<Ipv4Addr> {
    let segments = address.segments();
    let (high, low) = if segments[..6] == [0, 0, 0, 0, 0, 0] || segments[..6] == [0x0064, 0xff9b, 0, 0, 0, 0] {
        (segments[6], segments[7])
    } else if segments[0] == 0x2002 {
        (segments[1], segments[2])
    } else {
        return None;
    };
    let [a, b] = high.to_be_bytes();
    let [c, d] = low.to_be_bytes();
    Some(Ipv4Addr::new(a, b, c, d))
}

fn classify_ipv6(address: Ipv6Addr) -> ResourceDestination {
    let segments = address.segments();
    let embedded = address.to_ipv4_mapped().or_else(|| embedded_ipv4(address));
    if address.is_loopback()
        || address.is_unspecified()
        || address.is_multicast()
        || segments[0] & 0xffc0 == 0xfe80
        || embedded.is_some_and(|mapped| classify_ipv4(mapped) == ResourceDestination::Blocked)
    {
        ResourceDestination::Blocked
    } else if segments[0] & 0xfe00 == 0xfc00
        || segments[0] & 0xffc0 == 0xfec0
        || (segments[0] == 0x2001 && segments[1] == 0x0db8)
        || embedded.is_some_and(|mapped| classify_ipv4(mapped) == ResourceDestination::Private)
    {
        ResourceDestination::Private
    } else {
        ResourceDestination::Public
    }
}

/// Whether an address is publicly routable.
///
/// Derived from [`classify_ip`] so classification and this answer cannot drift apart.
pub fn is_public_ip(address: IpAddr) -> bool { classify_ip(address) == ResourceDestination::Public }

/// Strips the brackets of an IPv6 literal so that it parses as an address.
fn host_literal(host: &str) -> &str { host.strip_prefix('[').and_then(|value| value.strip_suffix(']')).unwrap_or(host) }

struct RememberedDestination {
    verdict: ResourceDestination,
    /// Whether the verdict came from a resolved answer. A name whose lookup produced none stays
    /// non-public, but nothing is known about it either.
    answered: bool,
    expires_at: Instant,
}

/// Memoizes destination verdicts so that rendering a playlist or EPG does not resolve the same host
/// per icon.
///
/// The verdict is a property of the host, so one memo serves the whole process: every route and
/// every rendering path has to reach the same answer, and a per-request memo would put a DNS lookup
/// on the playlist path for each request instead of once per host.
///
/// This is a hot-path memo, not policy state: the verdict decides whether a URL may be exposed, and
/// the resource client resolves and validates again while the connection is built, so a stale entry
/// can never open a connection to a local address.
struct DestinationCache {
    destinations: Mutex<LruCache<String, RememberedDestination>>,
}

impl DestinationCache {
    pub(super) fn new() -> Self {
        Self {
            destinations: Mutex::new(LruCache::new(
                NonZeroUsize::new(RESOURCE_DESTINATION_CACHE_CAPACITY).unwrap_or(NonZeroUsize::MIN),
            )),
        }
    }

    /// The process-wide memo.
    pub(super) fn shared() -> &'static Self {
        static SHARED: OnceLock<DestinationCache> = OnceLock::new();
        SHARED.get_or_init(DestinationCache::new)
    }

    fn verdict_and_answer(&self, host: &str) -> Option<(ResourceDestination, bool)> {
        let mut destinations = self.destinations.lock().unwrap_or_else(PoisonError::into_inner);
        match destinations.get(host) {
            Some(entry) if entry.expires_at > Instant::now() => Some((entry.verdict, entry.answered)),
            Some(_) => {
                destinations.pop(host);
                None
            }
            None => None,
        }
    }

    fn remember(&self, host: &str, verdict: ResourceDestination, answered: bool, ttl: Duration) {
        let mut destinations = self.destinations.lock().unwrap_or_else(PoisonError::into_inner);
        let entry = RememberedDestination { verdict, answered, expires_at: Instant::now() + ttl };
        destinations.put(host.to_owned(), entry);
    }
}

/// Classifies a destination and reports whether the verdict came from a resolved answer.
///
/// The second value separates "proven to be local" from "nothing was proven": both keep a client away
/// from the destination, but only the first says the destination cannot be reached from outside.
pub(crate) async fn classify_host(host: &str) -> (ResourceDestination, bool) {
    if let Ok(address) = host_literal(host).parse::<IpAddr>() {
        // An IP literal is decided by its address alone, so the answer is factual either way.
        return (classify_ip(address), true);
    }
    let cache = DestinationCache::shared();
    if let Some(verdict) = cache.verdict_and_answer(host) {
        return verdict;
    }

    let mut resolved = Vec::new();
    if let Ok(Ok(addresses)) = timeout(RESOURCE_DESTINATION_LOOKUP_TIMEOUT, tokio::net::lookup_host((host, 0))).await {
        resolved.extend(addresses.map(|address| address.ip()));
    }
    let (verdict, ttl) = verdict_for_addresses(&resolved);
    let answered = !resolved.is_empty();
    cache.remember(host, verdict, answered, ttl);
    (verdict, answered)
}

/// Aggregates the addresses a name resolved to into a single verdict, and the time that verdict may be
/// remembered.
///
/// One local-only address blocks the whole destination, and one private address keeps it non-public:
/// a client must not be able to reach the destination by any of its addresses. An empty answer is
/// non-public, because nothing was proven, and is remembered only briefly: unlike a resolved address,
/// it does not prove anything about the destination, so the name may well be reachable once the
/// resolver answers again.
pub(super) fn verdict_for_addresses(addresses: &[IpAddr]) -> (ResourceDestination, Duration) {
    if addresses.is_empty() {
        return (ResourceDestination::Private, RESOURCE_DESTINATION_UNANSWERED_TTL);
    }
    let mut verdict = ResourceDestination::Public;
    for address in addresses {
        match classify_ip(*address) {
            ResourceDestination::Blocked => {
                return (ResourceDestination::Blocked, RESOURCE_DESTINATION_PRIVATE_TTL);
            }
            ResourceDestination::Private => verdict = ResourceDestination::Private,
            ResourceDestination::Public => {}
        }
    }
    let ttl = match verdict {
        ResourceDestination::Public => RESOURCE_DESTINATION_PUBLIC_TTL,
        ResourceDestination::Private | ResourceDestination::Blocked => RESOURCE_DESTINATION_PRIVATE_TTL,
    };
    (verdict, ttl)
}
