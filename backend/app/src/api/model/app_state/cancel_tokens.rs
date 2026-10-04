use tokio_util::sync::CancellationToken;

pub struct CancelTokens {
    pub(crate) scheduler: CancellationToken,
    pub(crate) hdhomerun: CancellationToken,
    pub(crate) file_watch: CancellationToken,
    pub(crate) provider_dns: CancellationToken,
    pub(crate) metadata: CancellationToken,
    pub(crate) qos_aggregation: CancellationToken,
    pub(crate) recordings: CancellationToken,
    pub(crate) hls_cache: CancellationToken,
}
impl Default for CancelTokens {
    fn default() -> Self {
        Self {
            scheduler: CancellationToken::new(),
            hdhomerun: CancellationToken::new(),
            file_watch: CancellationToken::new(),
            provider_dns: CancellationToken::new(),
            metadata: CancellationToken::new(),
            qos_aggregation: CancellationToken::new(),
            recordings: CancellationToken::new(),
            hls_cache: CancellationToken::new(),
        }
    }
}
