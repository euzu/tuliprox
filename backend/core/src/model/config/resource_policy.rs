use crate::utils::{
    encode_base64_string,
    network::request::{canonicalize_ip, classify_ip, AddressClass},
};
use ipnet::IpNet;
use shared::{error::TuliproxError, model::ResourcePolicyDto};
use std::{
    fmt,
    hash::{Hash, Hasher},
    net::IpAddr,
    sync::{Arc, LazyLock},
};
use url::{Host, Url};

/// Domain separator for policy digests. Bumping it invalidates every persisted cache entry and
/// client identity that was derived from a policy.
const POLICY_DIGEST_DOMAIN: &[u8] = b"tuliprox.resource-policy.v1";

/// The networks a policy may authorize. Anything outside this list is either public or always
/// blocked, so accepting a broader range here could never grant access — it would only widen the
/// address space an operator can accidentally authorize.
const ALLOWED_PRIVATE_NETWORKS: &[&str] = &["10.0.0.0/8", "172.16.0.0/12", "192.168.0.0/16", "fc00::/7"];

/// Which destinations the resource proxy accepts for one input.
///
/// An empty policy is public-only and is the normalized representation of "no exception
/// configured". Normalization is part of the contract: two configurations that normalize to the
/// same hosts and networks are equal, share one HTTP client, and share one cache identity.
#[derive(Debug, Clone, Default)]
pub struct ResourcePolicy {
    pub allowed_hosts: Arc<[Arc<str>]>,
    pub allowed_networks: Arc<[IpNet]>,
}

impl PartialEq for ResourcePolicy {
    fn eq(&self, other: &Self) -> bool { self.digest() == other.digest() }
}

impl Eq for ResourcePolicy {}

impl Hash for ResourcePolicy {
    fn hash<H: Hasher>(&self, state: &mut H) { self.digest().hash(state); }
}

impl ResourcePolicy {
    #[inline]
    pub fn is_empty(&self) -> bool { self.allowed_hosts.is_empty() && self.allowed_networks.is_empty() }

    /// Stable identity of the normalized policy: the client key and the cache scope.
    pub fn digest(&self) -> PolicyDigest {
        let mut hasher = blake3::Hasher::new();
        hasher.update(POLICY_DIGEST_DOMAIN);
        hasher.update(&(self.allowed_hosts.len() as u64).to_be_bytes());
        for host in self.allowed_hosts.iter() {
            hasher.update(&(host.len() as u64).to_be_bytes());
            hasher.update(host.as_bytes());
        }
        hasher.update(&(self.allowed_networks.len() as u64).to_be_bytes());
        for network in self.allowed_networks.iter() {
            let network = network.to_string();
            hasher.update(&(network.len() as u64).to_be_bytes());
            hasher.update(network.as_bytes());
        }
        PolicyDigest(encode_base64_string(&hasher.finalize().as_bytes()[..16]).into())
    }

    /// Digest of the public-only policy. Every request without an exception uses it, so the
    /// cache scope of legacy and unconfigured inputs is one shared identity.
    pub fn public_only_digest() -> PolicyDigest { public_only_policy().digest() }

    /// Exact host name match, case-insensitive because host names are stored normalized.
    pub fn allows_host(&self, host: &str) -> bool { self.allowed_hosts.iter().any(|allowed| allowed.as_ref() == host) }

    pub fn allows_network(&self, address: IpAddr) -> bool {
        let address = canonicalize_ip(address);
        self.allowed_networks.iter().any(|network| network.contains(&address))
    }

    /// Decision for an address that came out of DNS resolution for `host`.
    pub fn authorize_resolved(&self, host: &str, address: IpAddr) -> Result<(), ResourcePolicyError> {
        match classify_ip(address) {
            AddressClass::Blocked => Err(ResourcePolicyError::BlockedAddress),
            AddressClass::Public => Ok(()),
            AddressClass::Private => {
                if !self.allows_host(host) {
                    return Err(ResourcePolicyError::HostNotTrusted);
                }
                if !self.allows_network(address) {
                    return Err(ResourcePolicyError::NetworkNotTrusted);
                }
                Ok(())
            }
        }
    }

    /// Decision for an IP literal inside a URL. A literal has no host name to match, so only the
    /// network policy can authorize it and the literal itself is the destination.
    pub fn authorize_literal(&self, address: IpAddr) -> Result<(), ResourcePolicyError> {
        match classify_ip(address) {
            AddressClass::Blocked => Err(ResourcePolicyError::BlockedAddress),
            AddressClass::Public => Ok(()),
            AddressClass::Private if self.allows_network(address) => Ok(()),
            AddressClass::Private => Err(ResourcePolicyError::NetworkNotTrusted),
        }
    }

    /// Validates scheme, host presence, and IP literals of a URL before the first request.
    ///
    /// Host names are deliberately not resolved here: `PolicyIpResolver` is the single
    /// connection-time enforcement point, so DNS rebinding protection and the single lookup stay
    /// intact. An IP literal never reaches the resolver, which is why it is classified here.
    pub fn validate_initial_url(&self, url: &Url) -> Result<(), ResourcePolicyError> {
        if !matches!(url.scheme(), "http" | "https") {
            return Err(ResourcePolicyError::UnsupportedScheme);
        }
        match url.host().ok_or(ResourcePolicyError::MissingHost)? {
            Host::Ipv4(address) => self.authorize_literal(IpAddr::V4(address)),
            Host::Ipv6(address) => self.authorize_literal(IpAddr::V6(address)),
            Host::Domain(_) => Ok(()),
        }
    }

    /// Normalizes and validates a configured policy.
    pub fn from_dto(dto: &ResourcePolicyDto) -> Result<Self, TuliproxError> {
        let allowed_hosts = normalize_hosts(&dto.allowed_hosts)?;
        let allowed_networks = normalize_networks(&dto.allowed_networks)?;
        Ok(Self { allowed_hosts: allowed_hosts.into(), allowed_networks: allowed_networks.into() })
    }

    /// Convenience for callers that hold a list of configured inputs.
    pub fn from_optional_dto(dto: Option<&ResourcePolicyDto>) -> Result<Self, TuliproxError> {
        dto.map_or_else(|| Ok(Self::default()), Self::from_dto)
    }
}

/// The shared public-only policy instance.
pub fn public_only_policy() -> Arc<ResourcePolicy> {
    static PUBLIC_ONLY: LazyLock<Arc<ResourcePolicy>> = LazyLock::new(|| Arc::new(ResourcePolicy::default()));
    Arc::clone(&PUBLIC_ONLY)
}

fn normalize_hosts(hosts: &[String]) -> Result<Vec<Arc<str>>, TuliproxError> {
    let mut normalized: Vec<String> = Vec::with_capacity(hosts.len());
    for host in hosts {
        let trimmed = host.trim();
        if trimmed.is_empty() {
            return Err(policy_error("host entry must not be empty"));
        }
        if trimmed.contains("://") || trimmed.contains('/') {
            return Err(policy_error(format!("'{trimmed}' is not a host name; remove scheme and path")));
        }
        if trimmed.contains('*') {
            return Err(policy_error(format!("wildcards are not supported in '{trimmed}'")));
        }
        if trimmed.contains(':') {
            return Err(policy_error(format!("'{trimmed}' must not contain a port")));
        }
        // A DNS name only. IP literals are authorized through `allowed_networks`, and accepting
        // them here would create a second, unchecked way to name a destination.
        match Host::parse(trimmed).map_err(|_| policy_error(format!("'{trimmed}' is not a valid host name")))? {
            Host::Domain(domain) => {
                let domain = domain.trim_end_matches('.').to_ascii_lowercase();
                if domain.is_empty() {
                    return Err(policy_error(format!("'{trimmed}' is not a valid host name")));
                }
                normalized.push(domain);
            }
            Host::Ipv4(_) | Host::Ipv6(_) => {
                return Err(policy_error(format!("'{trimmed}' is an IP address; list it in allowed_networks instead")))
            }
        }
    }
    normalized.sort_unstable();
    normalized.dedup();
    Ok(normalized.into_iter().map(Arc::from).collect())
}

fn normalize_networks(networks: &[String]) -> Result<Vec<IpNet>, TuliproxError> {
    let allowed: Vec<IpNet> =
        ALLOWED_PRIVATE_NETWORKS.iter().filter_map(|network| network.parse::<IpNet>().ok()).collect();
    let mut normalized: Vec<IpNet> = Vec::with_capacity(networks.len());
    for network in networks {
        let trimmed = network.trim();
        let parsed =
            trimmed.parse::<IpNet>().map_err(|_| policy_error(format!("'{trimmed}' is not a CIDR network")))?;
        let parsed = parsed.trunc();
        // The whole network must be contained in a private range, so a broad entry such as
        // 0.0.0.0/0 cannot silently authorize the public internet.
        if !is_within_private_range(&parsed, &allowed) {
            return Err(policy_error(format!(
                "'{trimmed}' is not contained in a private range ({})",
                ALLOWED_PRIVATE_NETWORKS.join(", ")
            )));
        }
        normalized.push(parsed);
    }
    normalized.sort_unstable();
    normalized.dedup();
    Ok(normalized)
}

fn is_within_private_range(network: &IpNet, allowed: &[IpNet]) -> bool {
    allowed.iter().any(|range| match (range, network) {
        (IpNet::V4(range), IpNet::V4(net)) => net.prefix_len() >= range.prefix_len() && range.contains(&net.network()),
        (IpNet::V6(range), IpNet::V6(net)) => net.prefix_len() >= range.prefix_len() && range.contains(&net.network()),
        _ => false,
    })
}

fn policy_error(message: impl Into<String>) -> TuliproxError {
    TuliproxError::ConfigInput(format!("invalid resource_policy: {}", message.into()))
}

/// Short, filesystem-safe identity of a normalized policy.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PolicyDigest(Arc<str>);

impl PolicyDigest {
    #[inline]
    pub fn as_str(&self) -> &str { &self.0 }
}

impl fmt::Display for PolicyDigest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { f.write_str(&self.0) }
}

/// Which redirect strategy a resource client uses. Both modes are needed at the same time: the
/// EPG route returns upstream redirects, the cached resource routes follow a bounded number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ResourceRedirectMode {
    NoRedirect,
    Bounded,
}

/// Identifies one resource HTTP client.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ResourceClientKey {
    pub policy_digest: PolicyDigest,
    pub redirect_mode: ResourceRedirectMode,
}

impl ResourceClientKey {
    pub fn new(policy_digest: PolicyDigest, redirect_mode: ResourceRedirectMode) -> Self {
        Self { policy_digest, redirect_mode }
    }
}

/// Why a resource request was refused. The variant is kept for diagnostics; the HTTP layer maps
/// every policy rejection to the same status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResourcePolicyError {
    /// The origin names an input that does not exist or is disabled.
    UnknownOrigin(String),
    BlockedAddress,
    HostNotTrusted,
    NetworkNotTrusted,
    TooManyRedirects,
    UnsupportedScheme,
    MissingHost,
}

impl fmt::Display for ResourcePolicyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownOrigin(name) => write!(f, "origin input '{name}' is unknown or disabled"),
            Self::BlockedAddress => f.write_str("destination is an always-blocked address"),
            Self::HostNotTrusted => f.write_str("private destination host is not listed in allowed_hosts"),
            Self::NetworkNotTrusted => f.write_str("destination address is not inside allowed_networks"),
            Self::TooManyRedirects => f.write_str("redirect limit exceeded"),
            Self::UnsupportedScheme => f.write_str("unsupported resource URL scheme"),
            Self::MissingHost => f.write_str("resource URL has no host"),
        }
    }
}

impl std::error::Error for ResourcePolicyError {}

#[cfg(test)]
mod tests {
    use super::{public_only_policy, ResourceClientKey, ResourcePolicy, ResourcePolicyDto, ResourceRedirectMode};
    use std::net::IpAddr;

    fn policy(hosts: &[&str], networks: &[&str]) -> ResourcePolicy {
        ResourcePolicy::from_dto(&ResourcePolicyDto {
            allowed_hosts: hosts.iter().map(|h| (*h).to_string()).collect(),
            allowed_networks: networks.iter().map(|n| (*n).to_string()).collect(),
        })
        .expect("policy should be valid")
    }

    fn ip(value: &str) -> IpAddr { value.parse().expect("ip address") }

    #[test]
    fn identical_policies_share_one_digest() {
        let first = policy(&["Media.Home.Arpa."], &["192.168.50.20/32", "10.0.0.0/8"]);
        let second = policy(&["media.home.arpa"], &["10.0.0.0/8", "192.168.50.20/32"]);
        let third = policy(&["media.home.arpa"], &["10.0.0.0/8", "192.168.50.16/28"]);

        assert_eq!(first.digest(), second.digest());
        assert_eq!(first, second);
        assert_ne!(first.digest(), third.digest());
    }

    #[test]
    fn empty_policy_is_public_only() {
        let empty = ResourcePolicy::default();
        assert!(empty.is_empty());
        assert_eq!(empty.digest(), public_only_policy().digest());
        assert_eq!(empty.authorize_resolved("any.host", ip("8.8.8.8")), Ok(()));
        assert_eq!(
            empty.authorize_resolved("any.host", ip("10.1.2.3")),
            Err(super::ResourcePolicyError::HostNotTrusted)
        );
        assert_eq!(empty.authorize_literal(ip("192.168.1.1")), Err(super::ResourcePolicyError::NetworkNotTrusted));
    }

    #[test]
    fn private_dns_destination_requires_host_and_network() {
        let policy = policy(&["media.home.arpa"], &["192.168.50.20/32"]);

        assert_eq!(policy.authorize_resolved("media.home.arpa", ip("192.168.50.20")), Ok(()));
        assert_eq!(
            policy.authorize_resolved("media.home.arpa", ip("192.168.50.21")),
            Err(super::ResourcePolicyError::NetworkNotTrusted)
        );
        assert_eq!(
            policy.authorize_resolved("other.home.arpa", ip("192.168.50.20")),
            Err(super::ResourcePolicyError::HostNotTrusted)
        );
    }

    #[test]
    fn ip_literals_are_authorized_by_network_only() {
        let policy = policy(&["media.home.arpa"], &["192.168.50.20/32"]);
        assert_eq!(policy.authorize_literal(ip("192.168.50.20")), Ok(()));
        assert_eq!(policy.authorize_literal(ip("192.168.50.21")), Err(super::ResourcePolicyError::NetworkNotTrusted));
        assert_eq!(policy.authorize_literal(ip("127.0.0.1")), Err(super::ResourcePolicyError::BlockedAddress));
        assert_eq!(policy.authorize_literal(ip("169.254.169.254")), Err(super::ResourcePolicyError::BlockedAddress));
    }

    #[test]
    fn mapped_ipv6_is_classified_as_its_ipv4_address() {
        let policy = policy(&["media.home.arpa"], &["192.168.50.20/32"]);
        assert_eq!(policy.authorize_literal(ip("::ffff:192.168.50.20")), Ok(()));
        assert_eq!(policy.authorize_literal(ip("::ffff:10.0.0.1")), Err(super::ResourcePolicyError::NetworkNotTrusted));
    }

    #[test]
    fn public_destinations_stay_allowed() {
        let policy = policy(&["media.home.arpa"], &["192.168.50.20/32"]);
        assert_eq!(policy.authorize_resolved("cdn.example.com", ip("93.184.216.34")), Ok(()));
        assert_eq!(policy.authorize_literal(ip("1.1.1.1")), Ok(()));
    }

    #[test]
    fn blocked_ranges_are_rejected_even_with_a_matching_network() {
        // 10.0.0.0/8 covers none of these, but the check must not depend on the policy at all.
        let policy = policy(&[], &["10.0.0.0/8"]);
        for blocked in ["127.0.0.1", "169.254.1.1", "0.0.0.0", "224.0.0.1", "255.255.255.255", "100.64.0.1"] {
            assert_eq!(
                policy.authorize_literal(ip(blocked)),
                Err(super::ResourcePolicyError::BlockedAddress),
                "{blocked} must stay blocked"
            );
        }
        for blocked in ["::1", "fe80::1", "ff02::1", "2001:db8::1"] {
            assert_eq!(
                policy.authorize_literal(ip(blocked)),
                Err(super::ResourcePolicyError::BlockedAddress),
                "{blocked} must stay blocked"
            );
        }
    }

    #[test]
    fn ula_addresses_are_private() {
        let policy = policy(&["nas.home.arpa"], &["fd00::/64"]);
        assert_eq!(policy.authorize_resolved("nas.home.arpa", ip("fd00::5")), Ok(()));
        assert_eq!(
            policy.authorize_resolved("nas.home.arpa", ip("fd00:1::5")),
            Err(super::ResourcePolicyError::NetworkNotTrusted)
        );
    }

    #[test]
    fn initial_url_validation_checks_scheme_host_and_literals() {
        let policy = policy(&["media.home.arpa"], &["192.168.50.20/32"]);

        let allowed = url::Url::parse("http://192.168.50.20/logo.png").expect("url");
        assert_eq!(policy.validate_initial_url(&allowed), Ok(()));

        let rejected = url::Url::parse("http://192.168.1.1/logo.png").expect("url");
        assert_eq!(policy.validate_initial_url(&rejected), Err(super::ResourcePolicyError::NetworkNotTrusted));

        let blocked = url::Url::parse("http://[::1]/logo.png").expect("url");
        assert_eq!(policy.validate_initial_url(&blocked), Err(super::ResourcePolicyError::BlockedAddress));

        // Host names are resolved later, by the connection-time resolver.
        let name = url::Url::parse("https://media.home.arpa/logo.png").expect("url");
        assert_eq!(policy.validate_initial_url(&name), Ok(()));

        let scheme = url::Url::parse("ftp://media.home.arpa/logo.png").expect("url");
        assert_eq!(policy.validate_initial_url(&scheme), Err(super::ResourcePolicyError::UnsupportedScheme));
    }

    #[test]
    fn host_entries_are_validated_and_normalized() {
        let dto = ResourcePolicyDto {
            allowed_hosts: vec!["Media.Home.Arpa.".to_string(), " media.home.arpa ".to_string()],
            allowed_networks: Vec::new(),
        };
        let policy = ResourcePolicy::from_dto(&dto).expect("policy");
        assert_eq!(policy.allowed_hosts.len(), 1);
        assert!(policy.allows_host("media.home.arpa"));

        for invalid in ["", "https://host/path", "host:8080", "*.home.arpa", "192.168.1.1"] {
            let dto = ResourcePolicyDto { allowed_hosts: vec![invalid.to_string()], allowed_networks: Vec::new() };
            assert!(ResourcePolicy::from_dto(&dto).is_err(), "'{invalid}' must be rejected");
        }
    }

    #[test]
    fn networks_outside_private_ranges_are_rejected() {
        for invalid in ["0.0.0.0/0", "8.8.8.0/24", "172.32.0.0/16", "2001:db8::/32", "not-a-network"] {
            let dto = ResourcePolicyDto { allowed_hosts: Vec::new(), allowed_networks: vec![invalid.to_string()] };
            assert!(ResourcePolicy::from_dto(&dto).is_err(), "'{invalid}' must be rejected");
        }

        for valid in ["10.0.0.0/8", "172.16.5.0/24", "192.168.50.20/32", "fc00::/7", "fd00::/64"] {
            let dto = ResourcePolicyDto { allowed_hosts: Vec::new(), allowed_networks: vec![valid.to_string()] };
            assert!(ResourcePolicy::from_dto(&dto).is_ok(), "'{valid}' must be accepted");
        }
    }

    #[tokio::test]
    async fn the_resolver_keeps_only_authorized_addresses() {
        use crate::utils::network::request::resolve_policy_socket_addrs;

        // A public literal is authorized by every policy, including the empty one.
        let addresses = resolve_policy_socket_addrs("1.1.1.1", 80, &ResourcePolicy::default())
            .await
            .expect("public literal resolves");
        assert_eq!(addresses.len(), 1);
        assert_eq!(addresses[0].port(), 80);

        // Loopback stays blocked even though the policy resolver is the only check here.
        let loopback = resolve_policy_socket_addrs("127.0.0.1", 80, &ResourcePolicy::default()).await;
        assert_eq!(
            loopback.err().map(|err| err.kind()),
            Some(std::io::ErrorKind::PermissionDenied),
            "loopback must be refused"
        );

        let private = resolve_policy_socket_addrs("192.168.1.1", 80, &ResourcePolicy::default()).await;
        assert_eq!(
            private.err().map(|err| err.kind()),
            Some(std::io::ErrorKind::PermissionDenied),
            "a private literal without a policy must be refused"
        );
    }

    #[test]
    fn loopback_can_never_be_authorized() {
        // Loopback is outside every allowed private range, so a policy that tries to authorize it
        // is rejected while the configuration is loaded instead of silently accepting it.
        let dto = ResourcePolicyDto {
            allowed_hosts: vec!["localhost".to_string()],
            allowed_networks: vec!["127.0.0.1/32".to_string()],
        };
        assert!(ResourcePolicy::from_dto(&dto).is_err());
    }

    #[test]
    fn client_keys_separate_policies_and_redirect_modes() {
        let first = ResourceClientKey::new(policy(&["a.home.arpa"], &[]).digest(), ResourceRedirectMode::Bounded);
        let same = ResourceClientKey::new(policy(&["a.home.arpa"], &[]).digest(), ResourceRedirectMode::Bounded);
        let other_mode =
            ResourceClientKey::new(policy(&["a.home.arpa"], &[]).digest(), ResourceRedirectMode::NoRedirect);
        let other_policy =
            ResourceClientKey::new(policy(&["b.home.arpa"], &[]).digest(), ResourceRedirectMode::Bounded);

        assert_eq!(first, same);
        assert_ne!(first, other_mode);
        assert_ne!(first, other_policy);
    }
}
