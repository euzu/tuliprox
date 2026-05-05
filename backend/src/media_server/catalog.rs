use crate::media_server::{
    MediaServerEpisode, MediaServerLibrary, MediaServerLibraryKind, MediaServerCatalogClient, MediaServerError, MediaServerErrorKind,
    MediaServerMovie, MediaServerPage, MediaServerPageRequest,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaServerCatalogCursor {
    pub library_id: String,
    pub kind: MediaServerLibraryKind,
    pub start: usize,
    pub limit: usize,
    pub total: Option<usize>,
    pub fetched: usize,
}

impl MediaServerCatalogCursor {
    pub fn from_page<T>(library: &MediaServerLibrary, page: &MediaServerPage<T>) -> Self {
        Self {
            library_id: library.reference.library_id.to_string(),
            kind: library.kind,
            start: page.request.start,
            limit: page.request.limit,
            total: page.total,
            fetched: page.upstream_item_count(),
        }
    }

    pub fn is_stalled_before_end(&self) -> bool {
        self.fetched == 0 && self.total.is_some_and(|total| self.start < total)
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct MediaServerCatalogRefreshPolicy {
    pub page_size: usize,
}

impl Default for MediaServerCatalogRefreshPolicy {
    fn default() -> Self { Self { page_size: 100 } }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MediaServerCatalogSnapshot {
    pub libraries: Vec<MediaServerLibrary>,
    pub movies: Vec<MediaServerMovie>,
    pub episodes: Vec<MediaServerEpisode>,
    pub unsupported_libraries: Vec<MediaServerLibrary>,
}

impl MediaServerCatalogSnapshot {
    pub fn item_count(&self) -> usize { self.movies.len() + self.episodes.len() }
}

#[derive(Debug, Clone, Default)]
pub struct MediaServerCatalogCache {
    trusted: Option<MediaServerCatalogSnapshot>,
}

impl MediaServerCatalogCache {
    pub fn trusted(&self) -> Option<&MediaServerCatalogSnapshot> { self.trusted.as_ref() }

    pub fn publish(&mut self, snapshot: MediaServerCatalogSnapshot) -> &MediaServerCatalogSnapshot {
        self.trusted.insert(snapshot)
    }

    pub async fn refresh_or_retain<C>(
        &mut self,
        client: &C,
        policy: MediaServerCatalogRefreshPolicy,
    ) -> MediaServerCatalogRefreshOutcome
    where
        C: MediaServerCatalogClient,
    {
        match refresh_media_server_catalog_complete_before_publish(client, policy).await {
            Ok(snapshot) => {
                self.publish(snapshot);
                MediaServerCatalogRefreshOutcome::Published
            }
            Err(error) => MediaServerCatalogRefreshOutcome::Retained { error, retained: self.trusted.is_some() },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MediaServerCatalogRefreshOutcome {
    Published,
    Retained { error: MediaServerError, retained: bool },
}

pub async fn refresh_media_server_catalog_complete_before_publish<C>(
    client: &C,
    policy: MediaServerCatalogRefreshPolicy,
) -> Result<MediaServerCatalogSnapshot, MediaServerError>
where
    C: MediaServerCatalogClient,
{
    if policy.page_size == 0 {
        return Err(MediaServerError::new(MediaServerErrorKind::MediaServerCatalogIncomplete)
            .detail("media server catalog page_size must be greater than zero"));
    }

    let _server = client.discover().await?;
    let libraries = client.list_libraries().await?;
    let mut snapshot = MediaServerCatalogSnapshot { libraries: libraries.clone(), ..MediaServerCatalogSnapshot::default() };

    for library in libraries {
        match library.kind {
            MediaServerLibraryKind::Movies => {
                let mut page_request = MediaServerPageRequest::new(0, policy.page_size);
                loop {
                    let page = client.list_movies(&library.reference, page_request).await?;
                    validate_page_progress(&library, &page)?;
                    let next_request = page.next_request();
                    snapshot.movies.extend(page.items);
                    let Some(next) = next_request else { break };
                    page_request = next;
                }
            }
            MediaServerLibraryKind::TvShows => {
                let mut page_request = MediaServerPageRequest::new(0, policy.page_size);
                loop {
                    let page = client.list_episodes(&library.reference, page_request).await?;
                    validate_page_progress(&library, &page)?;
                    let next_request = page.next_request();
                    snapshot.episodes.extend(page.items);
                    let Some(next) = next_request else { break };
                    page_request = next;
                }
            }
            MediaServerLibraryKind::Unsupported => snapshot.unsupported_libraries.push(library),
        }
    }

    Ok(snapshot)
}

fn validate_page_progress<T>(library: &MediaServerLibrary, page: &MediaServerPage<T>) -> Result<(), MediaServerError> {
    let cursor = MediaServerCatalogCursor::from_page(library, page);
    if cursor.is_stalled_before_end() {
        return Err(MediaServerError::new(MediaServerErrorKind::MediaServerCatalogPageStalled).detail(format!(
            "media server catalog page stalled for library kind {:?} at start {}",
            cursor.kind, cursor.start
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media_server::{
        MediaServerImageRef, MediaServerLibraryRef, MediaServerKind, MediaServerProviderIdHint,
        MediaServerResourceResponse, MediaServerStatus, MediaServerStreamRef, MediaServerStreamResponse,
    };
    use bytes::Bytes;
    use futures::{stream, StreamExt};
    use reqwest::{header::HeaderMap, StatusCode};
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct MockMediaServerCatalogClient {
        movie_pages: Mutex<Vec<Result<MediaServerPage<MediaServerMovie>, MediaServerError>>>,
        episode_pages: Mutex<Vec<Result<MediaServerPage<MediaServerEpisode>, MediaServerError>>>,
        libraries: Vec<MediaServerLibrary>,
    }

    impl MockMediaServerCatalogClient {
        fn with_libraries(libraries: Vec<MediaServerLibrary>) -> Self {
            Self { libraries, ..Self::default() }
        }
    }

    impl MediaServerCatalogClient for MockMediaServerCatalogClient {
        async fn discover(&self) -> Result<MediaServerStatus, MediaServerError> {
            Ok(MediaServerStatus {
                kind: MediaServerKind::Emby,
                server_id: "server-redacted".into(),
                display_name: None,
                version: None,
                owned: None,
            })
        }

        async fn list_libraries(&self) -> Result<Vec<MediaServerLibrary>, MediaServerError> { Ok(self.libraries.clone()) }

        async fn list_movies(
            &self,
            _library: &MediaServerLibraryRef,
            _page: MediaServerPageRequest,
        ) -> Result<MediaServerPage<MediaServerMovie>, MediaServerError> {
            self.movie_pages.lock().expect("lock").remove(0)
        }

        async fn list_episodes(
            &self,
            _library: &MediaServerLibraryRef,
            _page: MediaServerPageRequest,
        ) -> Result<MediaServerPage<MediaServerEpisode>, MediaServerError> {
            self.episode_pages.lock().expect("lock").remove(0)
        }

        async fn open_stream(
            &self,
            _stream_ref: &MediaServerStreamRef,
            _range: Option<&str>,
        ) -> Result<crate::media_server::MediaServerStreamResponse, MediaServerError> {
            Ok(empty_stream_response())
        }

        async fn open_image(
            &self,
            _image_ref: &MediaServerImageRef,
        ) -> Result<MediaServerResourceResponse, MediaServerError> {
            Ok(empty_response())
        }
    }

    fn empty_response() -> MediaServerResourceResponse {
        MediaServerResourceResponse { status: StatusCode::OK, headers: HeaderMap::new(), body: Bytes::new() }
    }

    fn empty_stream_response() -> MediaServerStreamResponse {
        MediaServerStreamResponse {
            status: StatusCode::OK,
            headers: HeaderMap::new(),
            body: stream::once(async { Ok::<Bytes, MediaServerError>(Bytes::new()) }).boxed(),
        }
    }

    fn movie_library() -> MediaServerLibrary {
        MediaServerLibrary {
            reference: MediaServerLibraryRef {
                input_name: "media_server".into(),
                server_id: "server".into(),
                library_id: "movies".into(),
            },
            name: "Movies".into(),
            kind: MediaServerLibraryKind::Movies,
        }
    }

    fn unsupported_library() -> MediaServerLibrary {
        MediaServerLibrary { kind: MediaServerLibraryKind::Unsupported, name: "Music".into(), ..movie_library() }
    }

    fn movie(id: &str) -> MediaServerMovie {
        MediaServerMovie {
            input_name: "media_server".into(),
            server_id: "server".into(),
            library_id: "movies".into(),
            item_id: Arc::<str>::from(id),
            title: Arc::<str>::from("Movie Redacted"),
            year: None,
            release_date: None,
            source_version_hint: None,
            provider_hints: Vec::<MediaServerProviderIdHint>::new(),
            technical_facts: None,
            stream_ref: None,
            image_ref: None,
        }
    }

    #[tokio::test]
    async fn incomplete_refresh_retains_previous_trusted_snapshot() {
        let mut cache = MediaServerCatalogCache::default();
        cache.publish(MediaServerCatalogSnapshot { movies: vec![movie("old")], ..MediaServerCatalogSnapshot::default() });

        let client = MockMediaServerCatalogClient::with_libraries(vec![movie_library()]);
        client.movie_pages.lock().expect("lock").extend([
            Ok(MediaServerPage::new(MediaServerPageRequest::new(0, 1), Some(2), vec![movie("new-1")])),
            Err(MediaServerError::new(MediaServerErrorKind::MediaServerUnavailable)),
        ]);

        let outcome = cache
            .refresh_or_retain(&client, MediaServerCatalogRefreshPolicy { page_size: 1 })
            .await;

        assert!(matches!(outcome, MediaServerCatalogRefreshOutcome::Retained { retained: true, .. }));
        assert_eq!(cache.trusted().expect("previous snapshot retained").movies[0].item_id.as_ref(), "old");
    }

    #[tokio::test]
    async fn stalled_page_returns_stable_failure() {
        let client = MockMediaServerCatalogClient::with_libraries(vec![movie_library()]);
        client
            .movie_pages
            .lock()
            .expect("lock")
            .push(Ok(MediaServerPage::new(MediaServerPageRequest::new(0, 100), Some(1), vec![])));

        let error = refresh_media_server_catalog_complete_before_publish(&client, MediaServerCatalogRefreshPolicy::default())
            .await
            .expect_err("stalled page should fail");

        assert_eq!(error.kind, MediaServerErrorKind::MediaServerCatalogPageStalled);
    }

    #[tokio::test]
    async fn unsupported_library_kind_is_reported_and_not_coerced() {
        let client = MockMediaServerCatalogClient::with_libraries(vec![unsupported_library()]);

        let snapshot = refresh_media_server_catalog_complete_before_publish(&client, MediaServerCatalogRefreshPolicy::default())
            .await
            .expect("unsupported library should be skipped safely");

        assert_eq!(snapshot.item_count(), 0);
        assert_eq!(snapshot.unsupported_libraries.len(), 1);
    }
}
