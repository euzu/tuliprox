use bytes::Bytes;
use reqwest::{header::HeaderMap, StatusCode};
use shared::model::InputType;
use std::sync::Arc;

#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub enum RemoteMediaServerKind {
    Emby,
    Jellyfin,
    Plex,
}

impl RemoteMediaServerKind {
    pub const fn as_input_type(self) -> InputType {
        match self {
            Self::Emby => InputType::Emby,
            Self::Jellyfin => InputType::Jellyfin,
            Self::Plex => InputType::Plex,
        }
    }
}

impl TryFrom<InputType> for RemoteMediaServerKind {
    type Error = &'static str;

    fn try_from(value: InputType) -> Result<Self, Self::Error> {
        match value {
            InputType::Emby => Ok(Self::Emby),
            InputType::Jellyfin => Ok(Self::Jellyfin),
            InputType::Plex => Ok(Self::Plex),
            InputType::M3u | InputType::Xtream | InputType::M3uBatch | InputType::XtreamBatch | InputType::Library => {
                Err("input type is not a remote media-server input")
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteServerStatus {
    pub kind: RemoteMediaServerKind,
    pub server_id: Arc<str>,
    pub display_name: Option<Arc<str>>,
    pub version: Option<Arc<str>>,
    pub owned: Option<bool>,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub enum RemoteLibraryKind {
    Movies,
    TvShows,
    Unsupported,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteLibraryRef {
    pub input_name: Arc<str>,
    pub server_id: Arc<str>,
    pub library_id: Arc<str>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteLibrary {
    pub reference: RemoteLibraryRef,
    pub name: Arc<str>,
    pub kind: RemoteLibraryKind,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct RemotePageRequest {
    pub start: usize,
    pub limit: usize,
}

impl RemotePageRequest {
    pub const fn new(start: usize, limit: usize) -> Self { Self { start, limit } }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemotePage<T> {
    pub request: RemotePageRequest,
    pub total: Option<usize>,
    pub upstream_item_count: usize,
    pub items: Vec<T>,
}

impl<T> RemotePage<T> {
    pub fn new(request: RemotePageRequest, total: Option<usize>, items: Vec<T>) -> Self {
        let upstream_item_count = items.len();
        Self { request, total, upstream_item_count, items }
    }

    pub fn with_upstream_item_count(
        request: RemotePageRequest,
        total: Option<usize>,
        upstream_item_count: usize,
        items: Vec<T>,
    ) -> Self {
        debug_assert!(
            upstream_item_count >= items.len(),
            "upstream_item_count must be greater than or equal to items.len()"
        );
        let upstream_item_count = upstream_item_count.max(items.len());
        Self { request, total, upstream_item_count, items }
    }

    pub fn item_count(&self) -> usize { self.items.len() }

    pub fn upstream_item_count(&self) -> usize { self.upstream_item_count }

    pub fn next_request(&self) -> Option<RemotePageRequest> {
        let next_start = self.request.start.saturating_add(self.upstream_item_count());
        if self.upstream_item_count() == 0 || self.total.is_some_and(|total| next_start >= total) {
            None
        } else {
            Some(RemotePageRequest::new(next_start, self.request.limit))
        }
    }

    pub fn cursor_advanced(&self) -> bool { self.upstream_item_count() > 0 }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteProviderIdHint {
    pub namespace: Arc<str>,
    pub value: Arc<str>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteMovie {
    pub input_name: Arc<str>,
    pub server_id: Arc<str>,
    pub library_id: Arc<str>,
    pub item_id: Arc<str>,
    pub title: Arc<str>,
    pub year: Option<u32>,
    pub source_version_hint: Option<Arc<str>>,
    pub provider_hints: Vec<RemoteProviderIdHint>,
    pub stream_ref: Option<RemoteStreamRef>,
    pub image_ref: Option<RemoteImageRef>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteEpisode {
    pub input_name: Arc<str>,
    pub server_id: Arc<str>,
    pub library_id: Arc<str>,
    pub item_id: Arc<str>,
    pub series_id: Option<Arc<str>>,
    pub series_title: Option<Arc<str>>,
    pub title: Arc<str>,
    pub season: Option<u32>,
    pub episode: Option<u32>,
    pub source_version_hint: Option<Arc<str>>,
    pub provider_hints: Vec<RemoteProviderIdHint>,
    pub stream_ref: Option<RemoteStreamRef>,
    pub image_ref: Option<RemoteImageRef>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteStreamRef {
    Emby {
        input_name: Arc<str>,
        server_id: Arc<str>,
        item_id: Arc<str>,
        media_source_id: Option<Arc<str>>,
    },
    Jellyfin {
        input_name: Arc<str>,
        server_id: Arc<str>,
        item_id: Arc<str>,
        media_source_id: Option<Arc<str>>,
    },
    Plex {
        input_name: Arc<str>,
        server_id: Arc<str>,
        rating_key: Arc<str>,
        part_key: Arc<str>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteImageRef {
    Emby {
        input_name: Arc<str>,
        server_id: Arc<str>,
        item_id: Arc<str>,
        image_kind: Arc<str>,
        tag: Option<Arc<str>>,
    },
    Jellyfin {
        input_name: Arc<str>,
        server_id: Arc<str>,
        item_id: Arc<str>,
        image_kind: Arc<str>,
        tag: Option<Arc<str>>,
    },
    Plex {
        input_name: Arc<str>,
        server_id: Arc<str>,
        rating_key: Arc<str>,
        image_path: Arc<str>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemotePlaybackLease {
    pub provider_kind: RemoteMediaServerKind,
    pub lease_id: Arc<str>,
}

#[derive(Debug, Clone)]
pub struct RemoteResourceResponse {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Bytes,
}

pub type RemoteStreamResponse = RemoteResourceResponse;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_page_next_request_advances_until_total() {
        let page = RemotePage::new(RemotePageRequest::new(0, 2), Some(3), vec![1, 2]);

        assert_eq!(page.next_request(), Some(RemotePageRequest::new(2, 2)));

        let last = RemotePage::new(RemotePageRequest::new(2, 2), Some(3), vec![3]);
        assert_eq!(last.next_request(), None);
    }

    #[test]
    fn remote_page_empty_page_does_not_advance() {
        let page = RemotePage::<u8>::new(RemotePageRequest::new(10, 100), Some(50), vec![]);

        assert!(!page.cursor_advanced());
        assert_eq!(page.next_request(), None);
    }

    #[test]
    fn remote_page_cursor_uses_upstream_count_when_items_are_filtered() {
        let page = RemotePage::with_upstream_item_count(RemotePageRequest::new(0, 3), Some(5), 3, vec![1]);

        assert_eq!(page.item_count(), 1);
        assert_eq!(page.upstream_item_count(), 3);
        assert!(page.cursor_advanced());
        assert_eq!(page.next_request(), Some(RemotePageRequest::new(3, 3)));
    }
}
