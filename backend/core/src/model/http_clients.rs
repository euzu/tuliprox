use arc_swap::ArcSwap;
use reqwest::Client;

/// The outbound HTTP clients, each swapped in place when the proxy or network
/// configuration is reloaded.
///
/// Subsystem contexts share the whole set behind one `Arc` rather than one
/// `Arc` per client.
pub struct HttpClients {
    /// Shared client that follows redirects.
    pub default: ArcSwap<Client>,
    /// Client that does not follow redirects, for provider requests.
    pub no_redirect: ArcSwap<Client>,
    /// The same, but connecting directly and refusing non-public destinations.
    pub public_no_redirect: ArcSwap<Client>,
    /// No-redirect client for a resource hop reachable only from the local network: connects directly
    /// and refuses addresses local to this host.
    pub resource_no_redirect: ArcSwap<Client>,
    /// No-redirect client for a resource hop that can leave the local network: honours the configured
    /// proxy and refuses addresses local to this host.
    pub resource_public_no_redirect: ArcSwap<Client>,
}

impl HttpClients {
    pub fn new(
        default: Client,
        no_redirect: Client,
        public_no_redirect: Client,
        resource_no_redirect: Client,
        resource_public_no_redirect: Client,
    ) -> Self {
        Self {
            default: ArcSwap::from_pointee(default),
            no_redirect: ArcSwap::from_pointee(no_redirect),
            public_no_redirect: ArcSwap::from_pointee(public_no_redirect),
            resource_no_redirect: ArcSwap::from_pointee(resource_no_redirect),
            resource_public_no_redirect: ArcSwap::from_pointee(resource_public_no_redirect),
        }
    }
}

impl Default for HttpClients {
    /// Unconfigured clients; for tests and contexts built without a server.
    fn default() -> Self { Self::new(Client::new(), Client::new(), Client::new(), Client::new(), Client::new()) }
}
