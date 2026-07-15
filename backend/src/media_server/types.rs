use crate::media_server::MediaServerError;
use bytes::Bytes;
use futures::stream::BoxStream;
use reqwest::{header::HeaderMap, StatusCode};
use shared::model::{InputType, MediaServerLibraryKind};
use std::{fmt, sync::Arc};

#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub enum MediaServerKind {
    Emby,
    Jellyfin,
    Plex,
}

impl MediaServerKind {
    pub const fn as_input_type(self) -> InputType {
        match self {
            Self::Emby => InputType::Emby,
            Self::Jellyfin => InputType::Jellyfin,
            Self::Plex => InputType::Plex,
        }
    }
}

impl TryFrom<InputType> for MediaServerKind {
    type Error = &'static str;

    fn try_from(value: InputType) -> Result<Self, Self::Error> {
        match value {
            InputType::Emby => Ok(Self::Emby),
            InputType::Jellyfin => Ok(Self::Jellyfin),
            InputType::Plex => Ok(Self::Plex),
            InputType::M3u
            | InputType::Xtream
            | InputType::M3uBatch
            | InputType::XtreamBatch
            | InputType::Stalker
            | InputType::StalkerBatch
            | InputType::Library
            | InputType::Staged => {
                Err("input type is not a media-server input")
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaServerStatus {
    pub kind: MediaServerKind,
    pub server_id: Arc<str>,
    pub display_name: Option<Arc<str>>,
    pub version: Option<Arc<str>>,
    pub owned: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaServerLibraryRef {
    pub input_name: Arc<str>,
    pub server_id: Arc<str>,
    pub library_id: Arc<str>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaServerLibrary {
    pub reference: MediaServerLibraryRef,
    pub name: Arc<str>,
    pub kind: MediaServerLibraryKind,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct MediaServerPageRequest {
    pub start: usize,
    pub limit: usize,
}

impl MediaServerPageRequest {
    pub const fn new(start: usize, limit: usize) -> Self { Self { start, limit } }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaServerPage<T> {
    pub request: MediaServerPageRequest,
    pub total: Option<usize>,
    pub upstream_item_count: usize,
    pub items: Vec<T>,
}

impl<T> MediaServerPage<T> {
    pub fn new(request: MediaServerPageRequest, total: Option<usize>, items: Vec<T>) -> Self {
        let upstream_item_count = items.len();
        Self { request, total, upstream_item_count, items }
    }

    pub fn with_upstream_item_count(
        request: MediaServerPageRequest,
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

    pub fn next_request(&self) -> Option<MediaServerPageRequest> {
        let next_start = self.request.start.saturating_add(self.upstream_item_count());
        if self.upstream_item_count() == 0 || self.total.is_some_and(|total| next_start >= total) {
            None
        } else {
            Some(MediaServerPageRequest::new(next_start, self.request.limit))
        }
    }

    pub fn cursor_advanced(&self) -> bool { self.upstream_item_count() > 0 }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaServerProviderIdHint {
    pub namespace: Arc<str>,
    pub value: Arc<str>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MediaServerDescriptiveFacts {
    pub original_title: Option<Arc<str>>,
    pub sort_title: Option<Arc<str>>,
    pub summary: Option<Arc<str>>,
    pub tagline: Option<Arc<str>>,
    pub studio: Option<Arc<str>>,
    pub network: Option<Arc<str>>,
    pub content_rating: Option<Arc<str>>,
    pub parental_age: Option<u32>,
    pub audience_rating: Option<Arc<str>>,
    pub genres: Vec<Arc<str>>,
    pub countries: Vec<Arc<str>>,
    pub directors: Vec<Arc<str>>,
    pub writers: Vec<Arc<str>>,
    pub cast: Vec<Arc<str>>,
    pub crew: Vec<Arc<str>>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MediaServerVideoTechnicalFacts {
    pub codec: Option<Arc<str>>,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MediaServerAudioTechnicalFacts {
    pub codec: Option<Arc<str>>,
    pub channels: Option<u32>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MediaServerTechnicalFacts {
    pub container: Option<Arc<str>>,
    pub duration_secs: Option<u32>,
    /// Source-declared bitrate normalized to bits per second (bps).
    pub bitrate: Option<u32>,
    pub video: Option<MediaServerVideoTechnicalFacts>,
    pub audio: Option<MediaServerAudioTechnicalFacts>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaServerMovie {
    pub input_name: Arc<str>,
    pub server_id: Arc<str>,
    pub library_id: Arc<str>,
    pub item_id: Arc<str>,
    pub title: Arc<str>,
    pub year: Option<u32>,
    pub release_date: Option<Arc<str>>,
    pub source_version_hint: Option<Arc<str>>,
    pub provider_hints: Vec<MediaServerProviderIdHint>,
    pub descriptive_facts: Option<MediaServerDescriptiveFacts>,
    pub technical_facts: Option<MediaServerTechnicalFacts>,
    pub stream_ref: Option<MediaServerStreamRef>,
    pub image_ref: Option<MediaServerImageRef>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaServerSeries {
    pub input_name: Arc<str>,
    pub server_id: Arc<str>,
    pub library_id: Arc<str>,
    pub item_id: Arc<str>,
    pub title: Arc<str>,
    pub year: Option<u32>,
    pub release_date: Option<Arc<str>>,
    pub source_version_hint: Option<Arc<str>>,
    pub provider_hints: Vec<MediaServerProviderIdHint>,
    pub descriptive_facts: Option<MediaServerDescriptiveFacts>,
    pub child_count: Option<u32>,
    pub episode_count: Option<u32>,
    pub image_ref: Option<MediaServerImageRef>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaServerSeason {
    pub input_name: Arc<str>,
    pub server_id: Arc<str>,
    pub library_id: Arc<str>,
    pub item_id: Arc<str>,
    pub series_id: Option<Arc<str>>,
    pub series_title: Option<Arc<str>>,
    pub title: Arc<str>,
    pub season: Option<u32>,
    pub year: Option<u32>,
    pub release_date: Option<Arc<str>>,
    pub source_version_hint: Option<Arc<str>>,
    pub provider_hints: Vec<MediaServerProviderIdHint>,
    pub descriptive_facts: Option<MediaServerDescriptiveFacts>,
    pub episode_count: Option<u32>,
    pub image_ref: Option<MediaServerImageRef>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaServerEpisode {
    pub input_name: Arc<str>,
    pub server_id: Arc<str>,
    pub library_id: Arc<str>,
    pub item_id: Arc<str>,
    pub series_id: Option<Arc<str>>,
    pub series_title: Option<Arc<str>>,
    pub title: Arc<str>,
    pub season: Option<u32>,
    pub episode: Option<u32>,
    pub release_date: Option<Arc<str>>,
    pub source_version_hint: Option<Arc<str>>,
    pub provider_hints: Vec<MediaServerProviderIdHint>,
    pub descriptive_facts: Option<MediaServerDescriptiveFacts>,
    pub technical_facts: Option<MediaServerTechnicalFacts>,
    pub stream_ref: Option<MediaServerStreamRef>,
    pub image_ref: Option<MediaServerImageRef>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MediaServerStreamRef {
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
pub enum MediaServerImageRef {
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
pub struct MediaServerPlaybackLease {
    pub provider_kind: MediaServerKind,
    pub lease_id: Arc<str>,
}

#[derive(Debug, Clone)]
pub struct MediaServerResourceResponse {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Bytes,
}

pub type BoxedMediaServerStream = BoxStream<'static, Result<Bytes, MediaServerError>>;

pub struct MediaServerStreamResponse {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: BoxedMediaServerStream,
}

impl fmt::Debug for MediaServerStreamResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MediaServerStreamResponse")
            .field("status", &self.status)
            .field("headers", &self.headers)
            .field("body", &"<stream>")
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media_server_page_next_request_advances_until_total() {
        let page = MediaServerPage::new(MediaServerPageRequest::new(0, 2), Some(3), vec![1, 2]);

        assert_eq!(page.next_request(), Some(MediaServerPageRequest::new(2, 2)));

        let last = MediaServerPage::new(MediaServerPageRequest::new(2, 2), Some(3), vec![3]);
        assert_eq!(last.next_request(), None);
    }

    #[test]
    fn media_server_page_empty_page_does_not_advance() {
        let page = MediaServerPage::<u8>::new(MediaServerPageRequest::new(10, 100), Some(50), vec![]);

        assert!(!page.cursor_advanced());
        assert_eq!(page.next_request(), None);
    }

    #[test]
    fn media_server_page_cursor_uses_upstream_count_when_items_are_filtered() {
        let page = MediaServerPage::with_upstream_item_count(MediaServerPageRequest::new(0, 3), Some(5), 3, vec![1]);

        assert_eq!(page.item_count(), 1);
        assert_eq!(page.upstream_item_count(), 3);
        assert!(page.cursor_advanced());
        assert_eq!(page.next_request(), Some(MediaServerPageRequest::new(3, 3)));
    }
}
