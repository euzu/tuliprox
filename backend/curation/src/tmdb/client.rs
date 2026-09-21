use super::model::translate_page;
use crate::kernel::CuratedMediaReference;
use reqwest::{
    header::{HeaderValue, ACCEPT, AUTHORIZATION},
    Client, Url,
};
use shared::model::{is_valid_tmdb_trending_limit, TmdbTrendingKind, TmdbTrendingTimeWindow};
use std::{
    collections::HashSet,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, ReadBuf},
    time::{timeout_at, Instant},
};
use tuliprox_core::{
    model::{TmdbCurationApiConfig, TmdbTrendingConfig},
    utils::network::content_coding::{decode_complete_response_to_identity, read_to_end_limited, ContentBodyReadError},
};

const TMDB_ORIGIN: &str = "https://api.themoviedb.org/";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const BODY_LIMIT: usize = 1024 * 1024;

/// No response text, dynamic URL, headers or underlying errors that could echo credentials.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) enum TmdbFailure {
    Configuration,
    Transport,
    Status(u16),
    Redirect,
    Body,
    BodyLimit,
    Deadline,
    RequestLimit,
    NoProgress,
    InvalidResponse,
}

/// Internal acquisition guards, independent of the configured reference count.
#[derive(Clone, Copy)]
struct Limits {
    request_timeout: Duration,
    request_bytes: usize,
    selector_timeout: Duration,
    selector_bytes: usize,
    selector_requests: usize,
    batch_timeout: Duration,
    batch_bytes: usize,
    batch_requests: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            request_timeout: REQUEST_TIMEOUT,
            request_bytes: BODY_LIMIT,
            selector_timeout: Duration::from_mins(1),
            selector_bytes: 8 * BODY_LIMIT,
            selector_requests: 32,
            batch_timeout: Duration::from_mins(3),
            batch_bytes: 32 * BODY_LIMIT,
            batch_requests: 128,
        }
    }
}

pub(super) struct AcquisitionBudget {
    deadline: Instant,
    requests: usize,
    bytes: usize,
    max_requests: usize,
    max_bytes: usize,
}

impl AcquisitionBudget {
    fn new(duration: Duration, max_requests: usize, max_bytes: usize) -> Self {
        Self { deadline: Instant::now() + duration, requests: 0, bytes: 0, max_requests, max_bytes }
    }

    fn check_time(&self) -> Result<(), TmdbFailure> {
        if Instant::now() >= self.deadline {
            Err(TmdbFailure::Deadline)
        } else {
            Ok(())
        }
    }

    fn admit(&self) -> Result<(), TmdbFailure> {
        self.check_time()?;
        if self.requests >= self.max_requests {
            return Err(TmdbFailure::RequestLimit);
        }
        if self.bytes >= self.max_bytes {
            return Err(TmdbFailure::BodyLimit);
        }
        Ok(())
    }
}

/// Account at the reader boundary, including partial failed bodies and the exact/+1 probe.
/// The outer timeout may drop a read future; already consumed bytes remain debited.
struct CountedReader<'a, R> {
    inner: R,
    selector_bytes: &'a mut usize,
    batch_bytes: &'a mut usize,
}

impl<R: AsyncRead + Unpin> AsyncRead for CountedReader<'_, R> {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        let before = buf.filled().len();
        let result = Pin::new(&mut this.inner).poll_read(cx, buf);
        let consumed = buf.filled().len() - before;
        *this.selector_bytes += consumed;
        *this.batch_bytes += consumed;
        result
    }
}

pub(crate) struct TmdbClient {
    http: Client,
    authorization: HeaderValue,
    origin: Url,
    limits: Limits,
}

impl TmdbClient {
    /// `http` must be the configured TMDB profile, never the generic/Trakt client.
    pub(crate) fn new(http: &Client, api: &TmdbCurationApiConfig) -> Result<Self, TmdbFailure> {
        let token = api.access_token.trim();
        if token.is_empty() {
            return Err(TmdbFailure::Configuration);
        }
        let mut authorization =
            HeaderValue::from_str(&format!("Bearer {token}")).map_err(|_| TmdbFailure::Configuration)?;
        authorization.set_sensitive(true);
        Ok(Self {
            http: http.clone(),
            authorization,
            origin: Url::parse(TMDB_ORIGIN).expect("fixed TMDB origin"),
            limits: Limits::default(),
        })
    }

    #[cfg(test)]
    pub(crate) fn for_test(http: &Client, api: &TmdbCurationApiConfig, origin: &str) -> Result<Self, TmdbFailure> {
        let mut client = Self::new(http, api)?;
        client.origin = Url::parse(origin).expect("local fixture origin");
        Ok(client)
    }

    pub(super) fn batch_budget(&self) -> AcquisitionBudget {
        AcquisitionBudget::new(self.limits.batch_timeout, self.limits.batch_requests, self.limits.batch_bytes)
    }

    pub(super) async fn trending(
        &self,
        selector: &TmdbTrendingConfig,
        batch: &mut AcquisitionBudget,
    ) -> Result<Vec<CuratedMediaReference>, TmdbFailure> {
        if !is_valid_tmdb_trending_limit(selector.limit) {
            return Err(TmdbFailure::Configuration);
        }
        let mut budget = AcquisitionBudget::new(
            self.limits.selector_timeout,
            self.limits.selector_requests,
            self.limits.selector_bytes,
        );
        let kind = match selector.kind {
            TmdbTrendingKind::Movie => "movie",
            TmdbTrendingKind::Tv => "tv",
        };
        let window = match selector.time_window {
            TmdbTrendingTimeWindow::Day => "day",
            TmdbTrendingTimeWindow::Week => "week",
        };
        let mut page_number = 1_u64;
        let mut preceding_rows = 0_u32;
        let mut seen = HashSet::new();
        let mut references = Vec::new();
        loop {
            budget.admit()?;
            batch.admit()?;
            let deadline = (Instant::now() + self.limits.request_timeout).min(budget.deadline).min(batch.deadline);
            let max_bytes =
                self.limits.request_bytes.min(budget.max_bytes - budget.bytes).min(batch.max_bytes - batch.bytes);
            let mut url = self.origin.join(&format!("3/trending/{kind}/{window}")).expect("fixed trending path");
            url.query_pairs_mut().append_pair("language", "en-US").append_pair("page", &page_number.to_string());
            // Debit before sending, including transport failures. The profile forbids redirects/replays.
            budget.requests += 1;
            batch.requests += 1;
            let body = timeout_at(deadline, async {
                let response = self
                    .http
                    .get(url.clone())
                    .header(AUTHORIZATION, self.authorization.clone())
                    .header(ACCEPT, "application/json")
                    .send()
                    .await
                    .map_err(|_| TmdbFailure::Transport)?;
                if response.status().is_redirection() || response.url() != &url {
                    return Err(TmdbFailure::Redirect);
                }
                if !response.status().is_success() {
                    return Err(TmdbFailure::Status(response.status().as_u16()));
                }
                let decoded = decode_complete_response_to_identity(response).await.map_err(|_| TmdbFailure::Body)?;
                let mut reader = CountedReader {
                    inner: decoded.body,
                    selector_bytes: &mut budget.bytes,
                    batch_bytes: &mut batch.bytes,
                };
                read_to_end_limited(&mut reader, max_bytes).await.map_err(|error| match error {
                    ContentBodyReadError::LimitExceeded { .. } => TmdbFailure::BodyLimit,
                    ContentBodyReadError::InvalidUtf8 { .. } | ContentBodyReadError::Io(_) => TmdbFailure::Body,
                })
            })
            .await
            .map_err(|_| TmdbFailure::Deadline)??;
            let page = translate_page(&body, selector.kind, page_number, preceding_rows)
                .map_err(|()| TmdbFailure::InvalidResponse)?;
            preceding_rows = preceding_rows
                .checked_add(u32::try_from(page.rows.len()).map_err(|_| TmdbFailure::InvalidResponse)?)
                .ok_or(TmdbFailure::InvalidResponse)?;
            let before = references.len();
            for reference in page.rows {
                if seen.insert(reference.tmdb_id.expect("validated positive ID")) {
                    references.push(reference);
                    if references.len() == selector.limit as usize {
                        break;
                    }
                }
            }
            // Deadlines include wire parsing/normalization, not local matching or publication.
            if Instant::now() >= deadline {
                return Err(TmdbFailure::Deadline);
            }
            if page_number > 1 && references.len() == before {
                return Err(TmdbFailure::NoProgress);
            }
            if references.len() == selector.limit as usize || page.last {
                return Ok(references);
            }
            page_number = page_number.checked_add(1).ok_or(TmdbFailure::InvalidResponse)?;
        }
    }
}

#[cfg(test)]
mod tests;
