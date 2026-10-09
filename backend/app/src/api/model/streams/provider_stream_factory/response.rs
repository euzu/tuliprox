use super::{
    error::{provider_decoded_body_error, ProviderStreamPreparationError},
    ProviderResponseHeadAvailability, ProviderStreamFactoryFlags, ProviderStreamFactoryOptions,
    ProviderStreamPreparationContext,
};
use crate::{
    api::model::{ProviderContentRepresentationMode, ProviderStreamFactoryResponse, StreamError, STREAM_IDLE_TIMEOUT},
    utils::content_coding::{decode_response_to_identity, ContentCodingDetection, ContentCodingError},
};
use futures::{StreamExt, TryStreamExt};
use reqwest::{
    header::{HeaderMap, HeaderValue, CONTENT_RANGE},
    StatusCode,
};
use std::{io, time::Duration};
use tokio_util::io::ReaderStream;
#[cfg(test)]
use tuliprox_hls::api::HlsOriginContentCodingObjectKind;
use tuliprox_hls::api::{
    extract_hls_provider_session_headers, log_hls_origin_content_coding, HlsOriginContentCodingSource,
};
use tuliprox_session::response_headers::provider_response_headers;
use url::Url;

#[cfg(test)]
pub(super) async fn prepare_provider_stream_response(
    response: reqwest::Response,
    mode: ProviderContentRepresentationMode,
    response_head_availability: ProviderResponseHeadAvailability,
) -> Result<ProviderStreamFactoryResponse, ProviderStreamPreparationError> {
    prepare_provider_stream_response_with_context(
        response,
        ProviderStreamPreparationContext {
            representation: mode,
            response_head_availability,
            range_requested: false,
            hls_content_coding_object_kind: matches!(mode, ProviderContentRepresentationMode::Identity)
                .then_some(HlsOriginContentCodingObjectKind::Other),
        },
        Duration::from_secs(STREAM_IDLE_TIMEOUT),
    )
    .await
}

#[cfg(test)]
pub(super) async fn prepare_provider_stream_response_with_idle_timeout(
    response: reqwest::Response,
    mode: ProviderContentRepresentationMode,
    response_head_availability: ProviderResponseHeadAvailability,
    idle_timeout: Duration,
) -> Result<ProviderStreamFactoryResponse, ProviderStreamPreparationError> {
    prepare_provider_stream_response_with_context(
        response,
        ProviderStreamPreparationContext {
            representation: mode,
            response_head_availability,
            range_requested: false,
            hls_content_coding_object_kind: matches!(mode, ProviderContentRepresentationMode::Identity)
                .then_some(HlsOriginContentCodingObjectKind::Other),
        },
        idle_timeout,
    )
    .await
}

/// Preserve specific upstream MIME types; repair missing or generic types for MP4 objects.
/// Audio and video share most fMP4 extensions, so audio is only recognized by an `m4a`/`cmfa`
/// extension or, when enabled for the input, by Flussonic's `tracks-a<N>` rendition paths.
pub(super) fn normalize_hls_resource_content_type(headers: &mut HeaderMap, url: &Url, flussonic_audio_tracks: bool) {
    let extension = url.path().rsplit_once('.').map_or("", |(_, ext)| ext);
    let mime = crate::utils::request::content_type_from_ext(extension);
    if !matches!(mime, "video/mp4" | "audio/mp4") {
        return;
    }
    let needs_type =
        headers.get(reqwest::header::CONTENT_TYPE).and_then(|value| value.to_str().ok()).is_none_or(|value| {
            let base = value.split(';').next().unwrap_or_default().trim();
            base.is_empty()
                || base.eq_ignore_ascii_case("application/octet-stream")
                || base.eq_ignore_ascii_case("video/mp2t")
        });
    if needs_type {
        // Flussonic names separate audio-only renditions tracks-a<index>.
        let audio_track = flussonic_audio_tracks
            && url.path_segments().is_some_and(|mut segments| {
                segments.any(|segment| {
                    segment
                        .strip_prefix("tracks-a")
                        .is_some_and(|index| !index.is_empty() && index.bytes().all(|byte| byte.is_ascii_digit()))
                })
            });
        headers.insert(
            reqwest::header::CONTENT_TYPE,
            HeaderValue::from_static(if audio_track { "audio/mp4" } else { mime }),
        );
    }
}

pub(super) async fn prepare_provider_stream_response_for_request(
    mut response: reqwest::Response,
    stream_options: &ProviderStreamFactoryOptions,
) -> Result<ProviderStreamFactoryResponse, ProviderStreamPreparationError> {
    if stream_options.flags.contains(ProviderStreamFactoryFlags::HlsResource) {
        normalize_hls_resource_content_type(
            response.headers_mut(),
            stream_options.get_url(),
            stream_options.flags.contains(ProviderStreamFactoryFlags::FlussonicAudioTracks),
        );
    }
    prepare_provider_stream_response_with_context(
        response,
        ProviderStreamPreparationContext {
            representation: stream_options.content_representation(),
            response_head_availability: stream_options.response_head_availability,
            range_requested: stream_options.was_range_requested(),
            hls_content_coding_object_kind: stream_options.hls_content_coding_object_kind,
        },
        Duration::from_secs(STREAM_IDLE_TIMEOUT),
    )
    .await
}

async fn prepare_provider_stream_response_with_context(
    response: reqwest::Response,
    context: ProviderStreamPreparationContext,
    idle_timeout: Duration,
) -> Result<ProviderStreamFactoryResponse, ProviderStreamPreparationError> {
    let provider_session_headers = extract_hls_provider_session_headers(response.headers());
    match context.representation {
        ProviderContentRepresentationMode::PreserveOrigin => {
            if matches!(context.response_head_availability, ProviderResponseHeadAvailability::Unavailable) {
                return Err(ProviderStreamPreparationError::DeferredResponseHead {
                    status: response.status(),
                    has_content_range: response.headers().contains_key(CONTENT_RANGE),
                });
            }
            let headers = provider_response_headers(response.headers(), context.representation)?;
            let response_info = Some((headers, response.status(), Some(response.url().clone()), None));
            let stream = response.bytes_stream().map_err(|error| StreamError::reqwest(&error)).boxed();
            Ok(ProviderStreamFactoryResponse {
                stream,
                info: response_info,
                provider_session_headers,
                has_upstream_owner: false,
            })
        }
        ProviderContentRepresentationMode::Identity => {
            let origin_status = response.status();
            let origin_has_content_range = response.headers().contains_key(CONTENT_RANGE);
            // `deflate` decoder selection reads a prefix before the returned stream can enter the
            // normal provider body wrappers, so decoder setup must own the same idle bound too.
            let decoded = tokio::time::timeout(
                idle_timeout,
                decode_response_to_identity(response, ContentCodingDetection::DeclaredOnly),
            )
            .await
            .map_err(|_| {
                ContentCodingError::PrefixRead(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "provider body idle timeout during content-decoder setup",
                ))
            })??;
            if matches!(context.response_head_availability, ProviderResponseHeadAvailability::Unavailable)
                && (origin_status != StatusCode::OK || origin_has_content_range)
            {
                return Err(ProviderStreamPreparationError::DeferredResponseHead {
                    status: origin_status,
                    has_content_range: origin_has_content_range,
                });
            }
            if let (Some(observation), Some(object_kind)) =
                (decoded.content_coding_observation(), context.hls_content_coding_object_kind)
            {
                log_hls_origin_content_coding(
                    observation,
                    object_kind,
                    context.range_requested,
                    HlsOriginContentCodingSource::Legacy,
                );
            }
            let headers = provider_response_headers(&decoded.headers, context.representation)?;
            let response_info = Some((headers, decoded.status, Some(decoded.final_url), None));
            let stream = ReaderStream::new(decoded.body).map_err(|error| provider_decoded_body_error(&error)).boxed();
            Ok(ProviderStreamFactoryResponse {
                stream,
                info: response_info,
                provider_session_headers,
                has_upstream_owner: false,
            })
        }
    }
}
