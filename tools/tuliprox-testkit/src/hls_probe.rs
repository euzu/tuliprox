//! Synthetic HLS timing and representation checks. These measure bytes, not decoded pictures.
use crate::{
    frame::{Frame, FrameDecoder, FrameValidator},
    protocol::{OriginStreamId, RunId},
    TestkitError,
};
use bytes::{Bytes, BytesMut};
use futures::{Stream, StreamExt};
use serde::{Deserialize, Serialize};
use std::{fmt, time::Duration};

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StartupMode {
    Conservative,
    FirstReady,
    Progressive,
}

impl fmt::Display for StartupMode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Conservative => "conservative",
            Self::FirstReady => "first_ready",
            Self::Progressive => "progressive",
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HlsOriginProfile {
    pub window_segments: u32,
    pub prefix_delay_millis: u64,
    pub missing_head_segments: u32,
    pub abort_segments: Vec<u64>,
    pub unknown_length: bool,
}

impl Default for HlsOriginProfile {
    fn default() -> Self {
        Self {
            window_segments: 6,
            prefix_delay_millis: 1000,
            missing_head_segments: 0,
            abort_segments: Vec::new(),
            unknown_length: false,
        }
    }
}

impl HlsOriginProfile {
    pub fn validate(&self) -> Result<(), TestkitError> {
        if !(3..=64).contains(&self.window_segments)
            || self.missing_head_segments >= self.window_segments
            || self.prefix_delay_millis > 10_000
            || self.abort_segments.len() > 64
        {
            return Err(TestkitError::Configuration(
                "HLS profile requires 3..64 segments, a shorter missing head, at most 64 aborts and delay <= 10000 ms"
                    .to_owned(),
            ));
        }
        Ok(())
    }
}

pub fn segment_parts(run: &RunId, marker: u32, segment: u64) -> Result<(Bytes, Bytes), TestkitError> {
    let first =
        segment.checked_mul(2).ok_or_else(|| TestkitError::Protocol("HLS frame sequence overflow".to_owned()))?;
    let second =
        first.checked_add(1).ok_or_else(|| TestkitError::Protocol("HLS frame sequence overflow".to_owned()))?;
    let stream = OriginStreamId::new(format!("hls-presentation-{marker}"));
    let mut prefix = BytesMut::new();
    let mut tail = BytesMut::new();
    Frame::synthetic(run, &stream, marker, first).encode(&mut prefix)?;
    Frame::synthetic(run, &stream, marker, second).encode(&mut tail)?;
    Ok((prefix.freeze(), tail.freeze()))
}

pub fn segment_stream(
    parts: (Bytes, Bytes),
    profile: &HlsOriginProfile,
    sequence: u64,
) -> impl Stream<Item = Result<Bytes, std::io::Error>> + Send + 'static {
    let delay = Duration::from_millis(profile.prefix_delay_millis);
    let abort = profile.abort_segments.contains(&sequence);
    futures::stream::once(async move { Ok(parts.0) }).chain(futures::stream::once(async move {
        tokio::time::sleep(delay).await;
        if abort {
            Err(std::io::Error::new(std::io::ErrorKind::ConnectionReset, "injected HLS origin body drop"))
        } else {
            Ok(parts.1)
        }
    }))
}

#[derive(Debug, Default)]
pub struct ProbeOptions {
    pub verify_revision: bool,
    pub expect_body_error: bool,
    pub max_first_byte_millis: Option<u64>,
    pub min_prefix_to_eof_millis: Option<u64>,
}

#[derive(Debug, Serialize)]
pub struct ProbeResult {
    pub entry_to_first_byte_millis: u64,
    pub entry_to_body_end_millis: u64,
    pub prefix_to_body_end_millis: u64,
    pub received_bytes: usize,
    pub valid_frames: u64,
    pub body_error: bool,
    pub retry_identical: bool,
    pub range_identical: bool,
}

const PROBE_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_BODY: usize = 1024 * 1024;

/// All requests share one absolute deadline. Errors after a prefix must be observable as errors.
pub async fn probe(url: &str, run: &RunId, marker: u32, options: &ProbeOptions) -> Result<ProbeResult, TestkitError> {
    if options.verify_revision && options.expect_body_error {
        return Err(TestkitError::Configuration("revision verification requires a complete body".to_owned()));
    }
    tokio::time::timeout(PROBE_TIMEOUT, probe_inner(url, run, marker, options))
        .await
        .map_err(|_| TestkitError::Protocol("HLS probe exceeded absolute deadline".to_owned()))?
}

async fn first_segment(client: &reqwest::Client, url: &str) -> Result<url::Url, TestkitError> {
    let mut url = url::Url::parse(url).map_err(|error| TestkitError::Configuration(error.to_string()))?;
    for _ in 0..8 {
        let response = client.get(url).send().await?.error_for_status()?;
        url = response.url().clone();
        let manifest = response.text().await?;
        let uri = manifest
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty() && !line.starts_with('#'))
            .ok_or_else(|| TestkitError::Protocol("HLS probe found no segment URI".to_owned()))?;
        url = url.join(uri).map_err(|error| TestkitError::Configuration(error.to_string()))?;
        if !manifest.lines().any(|line| line.starts_with("#EXT-X-STREAM-INF")) {
            return Ok(url);
        }
    }
    Err(TestkitError::Protocol("HLS probe master nesting limit".to_owned()))
}

async fn collect(response: reqwest::Response) -> Result<Bytes, TestkitError> {
    let mut body = response.bytes_stream();
    let mut data = BytesMut::new();
    while let Some(chunk) = body.next().await {
        let chunk = chunk?;
        if chunk.len() > MAX_BODY.saturating_sub(data.len()) {
            return Err(TestkitError::Protocol("HLS probe body exceeds 1 MiB limit".to_owned()));
        }
        data.extend_from_slice(&chunk);
    }
    Ok(data.freeze())
}

async fn verify_revision(client: &reqwest::Client, url: &url::Url, bytes: &[u8]) -> Result<(), TestkitError> {
    let retry = collect(client.get(url.clone()).send().await?.error_for_status()?).await?;
    if retry != bytes {
        return Err(TestkitError::Protocol("same HLS URI returned different retry bytes".to_owned()));
    }
    let end = bytes.len().saturating_sub(1).min(63);
    let response = client
        .get(url.clone())
        .header(reqwest::header::RANGE, format!("bytes=0-{end}"))
        .send()
        .await?
        .error_for_status()?;
    if response.status() != reqwest::StatusCode::PARTIAL_CONTENT {
        return Err(TestkitError::Protocol(
            "bound HLS revision did not return HTTP 206 for a completed range".to_owned(),
        ));
    }
    let expected = format!("bytes 0-{end}/{}", bytes.len());
    if response.headers().get(reqwest::header::CONTENT_RANGE).and_then(|value| value.to_str().ok())
        != Some(expected.as_str())
    {
        return Err(TestkitError::Protocol("bound HLS revision returned a different Content-Range".to_owned()));
    }
    if collect(response).await?.as_ref() != &bytes[..=end] {
        return Err(TestkitError::Protocol("HLS range offsets do not match the original revision".to_owned()));
    }
    Ok(())
}

async fn probe_inner(url: &str, run: &RunId, marker: u32, options: &ProbeOptions) -> Result<ProbeResult, TestkitError> {
    let started = tokio::time::Instant::now();
    let client = reqwest::Client::builder().http1_only().build()?;
    let segment_url = first_segment(&client, url).await?;
    let response = client.get(segment_url.clone()).send().await?.error_for_status()?;
    let expected_length = response.content_length();
    let mut stream = response.bytes_stream();
    let mut first_byte = None;
    let mut body_error = false;
    let mut bytes = BytesMut::new();
    while let Some(chunk) = stream.next().await {
        if let Ok(chunk) = chunk {
            if !chunk.is_empty() {
                first_byte.get_or_insert_with(tokio::time::Instant::now);
            }
            if chunk.len() > MAX_BODY.saturating_sub(bytes.len()) {
                return Err(TestkitError::Protocol("HLS probe body exceeds 1 MiB limit".to_owned()));
            }
            bytes.extend_from_slice(&chunk);
        } else {
            body_error = true;
            break;
        }
    }
    let ended = tokio::time::Instant::now();
    let first_byte = first_byte.ok_or_else(|| TestkitError::Protocol("HLS probe received no prefix".to_owned()))?;
    if body_error != options.expect_body_error {
        return Err(TestkitError::Protocol(format!(
            "HLS body error: expected {}, observed {body_error}",
            options.expect_body_error
        )));
    }
    if !body_error && expected_length.is_some_and(|length| length != bytes.len() as u64) {
        return Err(TestkitError::Protocol("HLS response ended before Content-Length".to_owned()));
    }
    let mut validator = FrameValidator::new(run, marker);
    let mut frames = 0;
    for frame in FrameDecoder::default().push(&bytes)? {
        validator.validate(&frame)?;
        frames += 1;
    }
    if frames == 0 {
        return Err(TestkitError::Protocol("HLS probe found no valid synthetic frame".to_owned()));
    }
    let millis = |duration: Duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX);
    let mut result = ProbeResult {
        entry_to_first_byte_millis: millis(first_byte - started),
        entry_to_body_end_millis: millis(ended - started),
        prefix_to_body_end_millis: millis(ended - first_byte),
        received_bytes: bytes.len(),
        valid_frames: frames,
        body_error,
        retry_identical: false,
        range_identical: false,
    };
    if options.max_first_byte_millis.is_some_and(|max| result.entry_to_first_byte_millis > max)
        || options.min_prefix_to_eof_millis.is_some_and(|min| result.prefix_to_body_end_millis < min)
    {
        return Err(TestkitError::Protocol("HLS probe timing assertion failed".to_owned()));
    }
    if options.verify_revision {
        verify_revision(&client, &segment_url, &bytes).await?;
        result.retry_identical = true;
        result.range_identical = true;
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::Body,
        http::{HeaderMap, StatusCode},
        response::IntoResponse,
        routing::get,
        Router,
    };

    async fn server(abort: bool, change_retry: bool) -> Result<(String, tokio::task::JoinHandle<()>), TestkitError> {
        let run = RunId::new("hls-probe-test");
        let parts = segment_parts(&run, 17, 0)?;
        let attempts = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let app = Router::new().route("/index.m3u8", get(|| async { "#EXTM3U\n#EXTINF:4,\n0.ts\n" })).route(
            "/0.ts",
            get(move |headers: HeaderMap| {
                let parts = parts.clone();
                let attempts = std::sync::Arc::clone(&attempts);
                async move {
                    if headers.contains_key("range") {
                        let mut response = (StatusCode::PARTIAL_CONTENT, parts.0.slice(..64)).into_response();
                        response.headers_mut().insert(
                            "content-range",
                            format!("bytes 0-63/{}", parts.0.len() + parts.1.len())
                                .parse()
                                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?,
                        );
                        return Ok::<_, StatusCode>(response);
                    }
                    let attempt = attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    if change_retry && attempt > 0 {
                        return Ok("changed revision".into_response());
                    }
                    let profile = HlsOriginProfile {
                        prefix_delay_millis: 100,
                        abort_segments: if abort { vec![0] } else { Vec::new() },
                        ..HlsOriginProfile::default()
                    };
                    Ok(Body::from_stream(segment_stream(parts, &profile, 0)).into_response())
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let url = format!("http://{}/index.m3u8", listener.local_addr()?);
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Ok((url, server))
    }

    #[tokio::test]
    async fn measures_prefix_before_eof_and_checks_retry_and_range() -> Result<(), TestkitError> {
        let (url, server) = server(false, false).await?;
        let result = probe(
            &url,
            &RunId::new("hls-probe-test"),
            17,
            &ProbeOptions { verify_revision: true, min_prefix_to_eof_millis: Some(50), ..ProbeOptions::default() },
        )
        .await;
        server.abort();
        let result = result?;
        assert_eq!(result.valid_frames, 2);
        assert!(result.retry_identical && result.range_identical);
        Ok(())
    }

    #[tokio::test]
    async fn truncated_chunked_body_is_an_error_after_valid_prefix() -> Result<(), TestkitError> {
        let (url, server) = server(true, false).await?;
        let result = probe(
            &url,
            &RunId::new("hls-probe-test"),
            17,
            &ProbeOptions { expect_body_error: true, ..ProbeOptions::default() },
        )
        .await;
        server.abort();
        assert!(result?.body_error);
        Ok(())
    }

    #[tokio::test]
    async fn detects_new_revision_on_same_uri() -> Result<(), TestkitError> {
        let (url, server) = server(false, true).await?;
        let result = probe(
            &url,
            &RunId::new("hls-probe-test"),
            17,
            &ProbeOptions { verify_revision: true, ..ProbeOptions::default() },
        )
        .await;
        server.abort();
        assert!(matches!(result, Err(TestkitError::Protocol(message)) if message.contains("different retry bytes")));
        Ok(())
    }

    #[test]
    fn rejects_unbounded_or_empty_profiles() {
        assert!(HlsOriginProfile { window_segments: 2, ..HlsOriginProfile::default() }.validate().is_err());
        assert!(HlsOriginProfile { missing_head_segments: 6, ..HlsOriginProfile::default() }.validate().is_err());
        assert!(HlsOriginProfile::default().validate().is_ok());
    }
}
