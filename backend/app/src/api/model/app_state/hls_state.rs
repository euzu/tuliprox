use crate::api::model::{HlsPlaylistHandoffCache, HlsProvisioningState};
use std::sync::Arc;
use tuliprox_hls::api::HlsProxyManager;

/// HLS request state the proxy manager does not own itself.
pub struct HlsState {
    /// Shared with [`crate::api::model::hls_cache::HlsCtx`], so it keeps its own `Arc`.
    pub proxy: Arc<HlsProxyManager>,
    pub provisioning: HlsProvisioningState,
    /// Entry media playlists handed off to the wrapped variant request.
    pub playlist_handoff: HlsPlaylistHandoffCache,
}

impl HlsState {
    pub fn new(proxy: Arc<HlsProxyManager>) -> Self {
        Self { proxy, provisioning: HlsProvisioningState::new(), playlist_handoff: HlsPlaylistHandoffCache::new() }
    }
}
