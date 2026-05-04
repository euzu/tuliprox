use crate::remote_media::{
    RemoteEpisode, RemoteLibrary, RemoteLibraryKind, RemoteMediaCatalogClient, RemoteMediaError, RemoteMediaErrorKind,
    RemoteMovie, RemotePage, RemotePageRequest,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteCatalogCursor {
    pub library_id: String,
    pub kind: RemoteLibraryKind,
    pub start: usize,
    pub limit: usize,
    pub total: Option<usize>,
    pub fetched: usize,
}

impl RemoteCatalogCursor {
    pub fn from_page<T>(library: &RemoteLibrary, page: &RemotePage<T>) -> Self {
        Self {
            library_id: library.reference.library_id.to_string(),
            kind: library.kind,
            start: page.request.start,
            limit: page.request.limit,
            total: page.total,
            fetched: page.item_count(),
        }
    }

    pub fn is_stalled_before_end(&self) -> bool {
        self.fetched == 0 && self.total.is_some_and(|total| self.start < total)
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct RemoteCatalogRefreshPolicy {
    pub page_size: usize,
}

impl Default for RemoteCatalogRefreshPolicy {
    fn default() -> Self { Self { page_size: 100 } }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RemoteCatalogSnapshot {
    pub libraries: Vec<RemoteLibrary>,
    pub movies: Vec<RemoteMovie>,
    pub episodes: Vec<RemoteEpisode>,
    pub unsupported_libraries: Vec<RemoteLibrary>,
}

impl RemoteCatalogSnapshot {
    pub fn item_count(&self) -> usize { self.movies.len() + self.episodes.len() }
}

#[derive(Debug, Clone, Default)]
pub struct RemoteCatalogCache {
    trusted: Option<RemoteCatalogSnapshot>,
}

impl RemoteCatalogCache {
    pub fn trusted(&self) -> Option<&RemoteCatalogSnapshot> { self.trusted.as_ref() }

    pub fn publish(&mut self, snapshot: RemoteCatalogSnapshot) -> &RemoteCatalogSnapshot {
        self.trusted = Some(snapshot);
        self.trusted.as_ref().expect("snapshot was just published")
    }

    pub async fn refresh_or_retain<C>(
        &mut self,
        client: &C,
        policy: RemoteCatalogRefreshPolicy,
    ) -> RemoteCatalogRefreshOutcome
    where
        C: RemoteMediaCatalogClient,
    {
        match refresh_remote_catalog_complete_before_publish(client, policy).await {
            Ok(snapshot) => {
                self.publish(snapshot);
                RemoteCatalogRefreshOutcome::Published
            }
            Err(error) => RemoteCatalogRefreshOutcome::Retained { error, retained: self.trusted.is_some() },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteCatalogRefreshOutcome {
    Published,
    Retained { error: RemoteMediaError, retained: bool },
}

pub async fn refresh_remote_catalog_complete_before_publish<C>(
    client: &C,
    policy: RemoteCatalogRefreshPolicy,
) -> Result<RemoteCatalogSnapshot, RemoteMediaError>
where
    C: RemoteMediaCatalogClient,
{
    if policy.page_size == 0 {
        return Err(RemoteMediaError::new(RemoteMediaErrorKind::RemoteCatalogIncomplete)
            .detail("remote catalog page_size must be greater than zero"));
    }

    let _server = client.discover().await?;
    let libraries = client.list_libraries().await?;
    let mut snapshot = RemoteCatalogSnapshot { libraries: libraries.clone(), ..RemoteCatalogSnapshot::default() };

    for library in libraries {
        match library.kind {
            RemoteLibraryKind::Movies => {
                let mut page_request = RemotePageRequest::new(0, policy.page_size);
                loop {
                    let page = client.list_movies(&library.reference, page_request).await?;
                    validate_page_progress(&library, &page)?;
                    let next_request = page.next_request();
                    snapshot.movies.extend(page.items);
                    let Some(next) = next_request else { break };
                    page_request = next;
                }
            }
            RemoteLibraryKind::TvShows => {
                let mut page_request = RemotePageRequest::new(0, policy.page_size);
                loop {
                    let page = client.list_episodes(&library.reference, page_request).await?;
                    validate_page_progress(&library, &page)?;
                    let next_request = page.next_request();
                    snapshot.episodes.extend(page.items);
                    let Some(next) = next_request else { break };
                    page_request = next;
                }
            }
            RemoteLibraryKind::Unsupported => snapshot.unsupported_libraries.push(library),
        }
    }

    Ok(snapshot)
}

fn validate_page_progress<T>(library: &RemoteLibrary, page: &RemotePage<T>) -> Result<(), RemoteMediaError> {
    let cursor = RemoteCatalogCursor::from_page(library, page);
    if cursor.is_stalled_before_end() {
        return Err(RemoteMediaError::new(RemoteMediaErrorKind::RemoteCatalogPageStalled).detail(format!(
            "remote catalog page stalled for library kind {:?} at start {}",
            cursor.kind, cursor.start
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote_media::{
        RemoteImageRef, RemoteLibraryRef, RemoteMediaServerKind, RemoteProviderIdHint, RemoteResourceResponse,
        RemoteServerStatus, RemoteStreamRef,
    };
    use bytes::Bytes;
    use reqwest::{header::HeaderMap, StatusCode};
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct MockRemoteCatalogClient {
        movie_pages: Mutex<Vec<Result<RemotePage<RemoteMovie>, RemoteMediaError>>>,
        episode_pages: Mutex<Vec<Result<RemotePage<RemoteEpisode>, RemoteMediaError>>>,
        libraries: Vec<RemoteLibrary>,
    }

    impl MockRemoteCatalogClient {
        fn with_libraries(libraries: Vec<RemoteLibrary>) -> Self {
            Self { libraries, ..Self::default() }
        }
    }

    impl RemoteMediaCatalogClient for MockRemoteCatalogClient {
        async fn discover(&self) -> Result<RemoteServerStatus, RemoteMediaError> {
            Ok(RemoteServerStatus {
                kind: RemoteMediaServerKind::Emby,
                server_id: "server-redacted".into(),
                display_name: None,
                version: None,
                owned: None,
            })
        }

        async fn list_libraries(&self) -> Result<Vec<RemoteLibrary>, RemoteMediaError> { Ok(self.libraries.clone()) }

        async fn list_movies(
            &self,
            _library: &RemoteLibraryRef,
            _page: RemotePageRequest,
        ) -> Result<RemotePage<RemoteMovie>, RemoteMediaError> {
            self.movie_pages.lock().expect("lock").remove(0)
        }

        async fn list_episodes(
            &self,
            _library: &RemoteLibraryRef,
            _page: RemotePageRequest,
        ) -> Result<RemotePage<RemoteEpisode>, RemoteMediaError> {
            self.episode_pages.lock().expect("lock").remove(0)
        }

        async fn open_stream(
            &self,
            _stream_ref: &RemoteStreamRef,
            _range: Option<&str>,
        ) -> Result<crate::remote_media::RemoteStreamResponse, RemoteMediaError> {
            Ok(empty_response())
        }

        async fn open_image(
            &self,
            _image_ref: &RemoteImageRef,
        ) -> Result<RemoteResourceResponse, RemoteMediaError> {
            Ok(empty_response())
        }
    }

    fn empty_response() -> RemoteResourceResponse {
        RemoteResourceResponse { status: StatusCode::OK, headers: HeaderMap::new(), body: Bytes::new() }
    }

    fn movie_library() -> RemoteLibrary {
        RemoteLibrary {
            reference: RemoteLibraryRef {
                input_name: "remote".into(),
                server_id: "server".into(),
                library_id: "movies".into(),
            },
            name: "Movies".into(),
            kind: RemoteLibraryKind::Movies,
        }
    }

    fn unsupported_library() -> RemoteLibrary {
        RemoteLibrary { kind: RemoteLibraryKind::Unsupported, name: "Music".into(), ..movie_library() }
    }

    fn movie(id: &str) -> RemoteMovie {
        RemoteMovie {
            input_name: "remote".into(),
            server_id: "server".into(),
            library_id: "movies".into(),
            item_id: Arc::<str>::from(id),
            title: Arc::<str>::from("Movie Redacted"),
            year: None,
            source_version_hint: None,
            provider_hints: Vec::<RemoteProviderIdHint>::new(),
            stream_ref: None,
            image_ref: None,
        }
    }

    #[tokio::test]
    async fn incomplete_refresh_retains_previous_trusted_snapshot() {
        let mut cache = RemoteCatalogCache::default();
        cache.publish(RemoteCatalogSnapshot { movies: vec![movie("old")], ..RemoteCatalogSnapshot::default() });

        let client = MockRemoteCatalogClient::with_libraries(vec![movie_library()]);
        client.movie_pages.lock().expect("lock").extend([
            Ok(RemotePage {
                request: RemotePageRequest::new(0, 1),
                total: Some(2),
                items: vec![movie("new-1")],
            }),
            Err(RemoteMediaError::new(RemoteMediaErrorKind::RemoteServerUnavailable)),
        ]);

        let outcome = cache
            .refresh_or_retain(&client, RemoteCatalogRefreshPolicy { page_size: 1 })
            .await;

        assert!(matches!(outcome, RemoteCatalogRefreshOutcome::Retained { retained: true, .. }));
        assert_eq!(cache.trusted().expect("previous snapshot retained").movies[0].item_id.as_ref(), "old");
    }

    #[tokio::test]
    async fn stalled_page_returns_stable_failure() {
        let client = MockRemoteCatalogClient::with_libraries(vec![movie_library()]);
        client.movie_pages.lock().expect("lock").push(Ok(RemotePage {
            request: RemotePageRequest::new(0, 100),
            total: Some(1),
            items: vec![],
        }));

        let error = refresh_remote_catalog_complete_before_publish(&client, RemoteCatalogRefreshPolicy::default())
            .await
            .expect_err("stalled page should fail");

        assert_eq!(error.kind, RemoteMediaErrorKind::RemoteCatalogPageStalled);
    }

    #[tokio::test]
    async fn unsupported_library_kind_is_reported_and_not_coerced() {
        let client = MockRemoteCatalogClient::with_libraries(vec![unsupported_library()]);

        let snapshot = refresh_remote_catalog_complete_before_publish(&client, RemoteCatalogRefreshPolicy::default())
            .await
            .expect("unsupported library should be skipped safely");

        assert_eq!(snapshot.item_count(), 0);
        assert_eq!(snapshot.unsupported_libraries.len(), 1);
    }
}
