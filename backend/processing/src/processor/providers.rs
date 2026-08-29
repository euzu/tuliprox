//! The [`PlaylistProvider`] implementations whose orchestration lives in this crate.
//!
//! The M3U and Xtream providers ship with the client code in `tuliprox-iptv`. These three
//! cannot: Stalker's resumable refresh, the local library scan and the Plex catalog import
//! all reach into `tuliprox-repository` and this crate's own processors, and `tuliprox-iptv`
//! sits below both. The trait is what lets them keep that dependency direction and still
//! answer the dispatcher in one shape.

use super::{
    library::download_library_playlist, stalker::download_stalker_playlist, stalker_refresh::StalkerRefreshMode,
};
use crate::{metadata_sink::MetadataUpdateSink, parser::xmltv::TVGuide, processor::PlaylistProcessingContext};
use shared::{
    error::TuliproxError,
    model::{EventSink, PlaylistGroup},
};
use tuliprox_core::model::ConfigInput;
use tuliprox_iptv::{
    epg::{EpgFetchRequest, EpgOutcome, EpgProvider, EpgRecordSink},
    provider::{PlaylistFetch, PlaylistFetchRequest, PlaylistProvider},
};
use tuliprox_media_server::{
    media_server_catalog_snapshot_to_playlist, refresh_media_server_catalog_complete_before_publish,
    MediaServerCatalogRefreshPolicy, MediaServerHttpClient,
};

/// Stalker/Ministra portal. Carries the refresh mode and whether the fetched generation
/// should be published immediately, both of which only this provider has.
pub struct StalkerProvider {
    refresh_mode: StalkerRefreshMode,
    materialize_active: bool,
}

impl StalkerProvider {
    pub fn new(refresh_mode: StalkerRefreshMode, materialize_active: bool) -> Self {
        Self { refresh_mode, materialize_active }
    }
}

impl PlaylistProvider for StalkerProvider {
    fn name(&self) -> &'static str { "stalker" }

    async fn fetch(&self, request: &PlaylistFetchRequest<'_>) -> PlaylistFetch {
        let (groups, errors, persisted, partial) = download_stalker_playlist(
            request.app_config,
            request.client,
            request.input,
            None,
            self.refresh_mode,
            self.materialize_active,
        )
        .await;
        PlaylistFetch::groups(groups).with_errors(errors).persisted(persisted).partial(partial)
    }
}

/// Local media library scan.
#[derive(Debug, Clone, Copy, Default)]
pub struct LibraryProvider;

impl PlaylistProvider for LibraryProvider {
    fn name(&self) -> &'static str { "library" }

    async fn fetch(&self, request: &PlaylistFetchRequest<'_>) -> PlaylistFetch {
        let (groups, errors) = download_library_playlist(request.client, request.app_config, request.input).await;
        PlaylistFetch::groups(groups).with_errors(errors)
    }
}

/// Plex media server catalog import.
#[derive(Debug, Clone, Copy, Default)]
pub struct PlexProvider;

impl PlaylistProvider for PlexProvider {
    fn name(&self) -> &'static str { "plex" }

    async fn fetch(&self, request: &PlaylistFetchRequest<'_>) -> PlaylistFetch {
        let (groups, errors) = download_plex_media_server_playlist(request.client, request.input).await;
        PlaylistFetch::groups(groups).with_errors(errors)
    }
}

async fn download_plex_media_server_playlist(
    client: &reqwest::Client,
    input: &ConfigInput,
) -> (Vec<PlaylistGroup>, Vec<TuliproxError>) {
    let Some(media_server) = input.media_server.as_ref() else {
        return (
            vec![],
            vec![TuliproxError::Download(format!(
                "media-server input '{}' is missing media_server configuration",
                input.name
            ))],
        );
    };
    let http_client = MediaServerHttpClient::new(client.clone());
    let plex_client = match input.plex_catalog_client(http_client) {
        Ok(client) => client,
        Err(error) => return (vec![], vec![TuliproxError::Download(error.to_string())]),
    };
    let policy = MediaServerCatalogRefreshPolicy {
        page_size: usize::from(media_server.catalog.page_size),
        request_delay_ms: media_server.catalog.request_delay_ms,
    };

    match refresh_media_server_catalog_complete_before_publish(&plex_client, policy).await {
        Ok(snapshot) => (media_server_catalog_snapshot_to_playlist(&snapshot), vec![]),
        Err(error) => (vec![], vec![TuliproxError::Download(error.to_string())]),
    }
}

/// XMLTV/ICS EPG for M3U, Xtream and library inputs.
///
/// The counterpart to `tuliprox_iptv::epg::StalkerEpgProvider`: this one downloads whole
/// documents and hands back a [`TVGuide`] over the files, rather than streaming programme
/// records. Which of the two happened is exactly what
/// [`EpgOutcome`](tuliprox_iptv::epg::EpgOutcome) exists to say - forcing a
/// several-hundred-megabyte XMLTV document through a record sink would mean materialising
/// all of it in memory to answer a question nobody asked.
pub struct XmltvEpgProvider<'a, E: EventSink + Clone + 'static, M: MetadataUpdateSink> {
    ctx: &'a PlaylistProcessingContext<E, M>,
    errors: parking_lot::Mutex<Vec<TuliproxError>>,
}

impl<'a, E: EventSink + Clone + 'static, M: MetadataUpdateSink> XmltvEpgProvider<'a, E, M> {
    pub fn new(ctx: &'a PlaylistProcessingContext<E, M>) -> Self {
        Self { ctx, errors: parking_lot::Mutex::new(Vec::new()) }
    }

    /// The per-source failures collected during the fetch.
    ///
    /// EPG download is per-source and partial by nature: three sources where one 404s
    /// still yields a usable guide from the other two, so the failures travel alongside
    /// the outcome rather than replacing it.
    pub fn take_errors(&self) -> Vec<TuliproxError> { std::mem::take(&mut *self.errors.lock()) }
}

impl<E: EventSink + Clone + 'static, M: MetadataUpdateSink> EpgProvider for XmltvEpgProvider<'_, E, M> {
    type Guide = TVGuide;

    fn name(&self) -> &'static str { "xmltv" }

    async fn fetch(
        &self,
        request: &EpgFetchRequest<'_>,
        _sink: &mut impl EpgRecordSink,
    ) -> Result<EpgOutcome<TVGuide>, TuliproxError> {
        if request.input.epg.is_none() {
            return Ok(EpgOutcome::NotConfigured);
        }
        let storage_dir = { self.ctx.config.config.load().storage_dir.clone() };
        let (guide, errors) = crate::epg::get_xmltv(self.ctx, request.input, None, &storage_dir).await;
        self.errors.lock().extend(errors);
        Ok(guide.map_or(EpgOutcome::NotConfigured, EpgOutcome::Guide))
    }
}
