use crate::{
    api::model::{ProviderContentRepresentationMode, ProviderStreamHeader},
    utils::content_coding::parse_content_encoding_tokens,
};
use reqwest::{
    header::{HeaderMap, CONTENT_ENCODING, CONTENT_LENGTH, CONTENT_RANGE, TRANSFER_ENCODING},
    StatusCode,
};
use shared::utils::filter_response_header;
use std::{collections::HashSet, str::FromStr};

pub fn get_response_headers(headers: &HeaderMap) -> Vec<(String, String)> {
    headers
        .iter()
        .filter(|(key, _)| filter_response_header(key.as_str()))
        .filter_map(|(key, value)| value.to_str().ok().map(|v| (key.to_string(), v.to_string())))
        .collect()
}

/// Failures raised while selecting representation-sensitive provider response headers.
#[allow(clippy::enum_variant_names)]
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub(crate) enum ProviderResponseHeaderError {
    #[error("invalid Content-Encoding header")]
    InvalidContentEncoding,

    #[error("invalid Content-Length header")]
    InvalidContentLength,

    #[error("invalid Content-Range header")]
    InvalidContentRange,
}

/// Selects representation-consistent provider headers without widening the global response allowlist.
pub(crate) fn provider_response_headers(
    headers: &HeaderMap,
    mode: ProviderContentRepresentationMode,
) -> Result<ProviderStreamHeader, ProviderResponseHeaderError> {
    let mut selected = headers
        .iter()
        .filter(|(key, _)| {
            let name = key.as_str();
            name != CONTENT_ENCODING.as_str()
                && name != CONTENT_LENGTH.as_str()
                && name != CONTENT_RANGE.as_str()
                && name != TRANSFER_ENCODING.as_str()
        })
        .filter(|(key, _)| filter_response_header(key.as_str()))
        .filter_map(|(key, value)| value.to_str().ok().map(|value| (key.to_string(), value.to_string())))
        .collect::<Vec<_>>();

    if matches!(mode, ProviderContentRepresentationMode::PreserveOrigin) {
        let content_encoding =
            parse_content_encoding_tokens(headers).map_err(|_| ProviderResponseHeaderError::InvalidContentEncoding)?;
        if !content_encoding.is_empty() {
            selected.push((CONTENT_ENCODING.as_str().to_owned(), content_encoding.join(", ")));
        }
    }
    if let Some(content_length) = validated_content_length(headers)? {
        selected.push((CONTENT_LENGTH.as_str().to_owned(), content_length));
    }
    if let Some(content_range) = validated_content_range(headers)? {
        selected.push((CONTENT_RANGE.as_str().to_owned(), content_range));
    }

    Ok(selected)
}

fn validated_content_length(headers: &HeaderMap) -> Result<Option<String>, ProviderResponseHeaderError> {
    let mut content_length = None;

    for value in headers.get_all(CONTENT_LENGTH) {
        let value = value.to_str().map_err(|_| ProviderResponseHeaderError::InvalidContentLength)?;
        for value in value.split(',') {
            let value = value.trim_matches(|character| matches!(character, ' ' | '\t'));
            if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err(ProviderResponseHeaderError::InvalidContentLength);
            }
            let value = value.parse::<u64>().map_err(|_| ProviderResponseHeaderError::InvalidContentLength)?;
            match content_length {
                Some(expected) if value != expected => {
                    return Err(ProviderResponseHeaderError::InvalidContentLength);
                }
                Some(_) => {}
                None => content_length = Some(value),
            }
        }
    }

    Ok(content_length.map(|value| value.to_string()))
}

fn validated_content_range(headers: &HeaderMap) -> Result<Option<String>, ProviderResponseHeaderError> {
    let mut values = headers.get_all(CONTENT_RANGE).iter();
    let Some(value) = values.next() else {
        return Ok(None);
    };
    if values.next().is_some() {
        return Err(ProviderResponseHeaderError::InvalidContentRange);
    }

    let value = value.to_str().map_err(|_| ProviderResponseHeaderError::InvalidContentRange)?;
    let value = value.trim_matches(|character| matches!(character, ' ' | '\t'));
    if value.is_empty() || value.contains(',') {
        return Err(ProviderResponseHeaderError::InvalidContentRange);
    }
    Ok(Some(value.to_owned()))
}

pub fn get_stream_response_with_headers(
    custom: Option<(Vec<(String, String)>, StatusCode)>,
) -> (axum::http::StatusCode, axum::http::HeaderMap) {
    let mut headers = HeaderMap::new();
    let mut added_headers: HashSet<String> = HashSet::new();
    let mut status = StatusCode::OK;

    if let Some((custom_headers, status_code)) = custom {
        status = status_code;
        for (key, value) in custom_headers {
            if key.eq_ignore_ascii_case(TRANSFER_ENCODING.as_str()) {
                continue;
            }
            if let (Ok(name), Ok(val)) =
                (axum::http::HeaderName::from_str(&key), axum::http::HeaderValue::from_str(&value))
            {
                headers.insert(name.clone(), val);
                added_headers.insert(key);
            }
        }
    }

    let default_headers = vec![("content-type", "application/octet-stream")];

    for (key, value) in default_headers {
        if !added_headers.contains(key) {
            if let (Ok(name), Ok(val)) =
                (axum::http::HeaderName::from_str(key), axum::http::HeaderValue::from_str(value))
            {
                headers.insert(name, val);
            }
        }
    }

    if let Ok(date_header) = axum::http::HeaderValue::from_str(&chrono::Utc::now().to_rfc2822()) {
        headers.insert(axum::http::HeaderName::from_static("date"), date_header);
    }

    (status, headers)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils::content_coding::normalize_headers_after_content_decoding;
    use reqwest::header::{
        ACCEPT_RANGES, AUTHORIZATION, CACHE_CONTROL, CONTENT_TYPE, ETAG, PROXY_AUTHENTICATE, SET_COOKIE,
        WWW_AUTHENTICATE,
    };

    fn selected_value<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
        headers.iter().find_map(|(key, value)| key.eq_ignore_ascii_case(name).then_some(value.as_str()))
    }

    #[test]
    fn provider_preserve_origin_keeps_representation_headers_and_drops_unsafe_headers() {
        let mut headers = HeaderMap::new();
        headers.append(CONTENT_ENCODING, "gzip".parse().unwrap());
        headers.append(CONTENT_ENCODING, "br".parse().unwrap());
        headers.insert(CONTENT_LENGTH, "321".parse().unwrap());
        headers.insert(CONTENT_RANGE, "bytes 10-330/1000".parse().unwrap());
        headers.insert(TRANSFER_ENCODING, "chunked".parse().unwrap());
        headers.insert(SET_COOKIE, "provider_session=secret".parse().unwrap());
        headers.insert(AUTHORIZATION, "Bearer secret".parse().unwrap());
        headers.insert("x-provider-secret", "secret".parse().unwrap());

        let selected = provider_response_headers(&headers, ProviderContentRepresentationMode::PreserveOrigin)
            .expect("valid provider headers");

        assert_eq!(selected_value(&selected, CONTENT_ENCODING.as_str()), Some("gzip, br"));
        assert_eq!(selected_value(&selected, CONTENT_LENGTH.as_str()), Some("321"));
        assert_eq!(selected_value(&selected, CONTENT_RANGE.as_str()), Some("bytes 10-330/1000"));
        for rejected in [TRANSFER_ENCODING.as_str(), SET_COOKIE.as_str(), AUTHORIZATION.as_str(), "x-provider-secret"] {
            assert_eq!(selected_value(&selected, rejected), None, "unexpected header {rejected}");
        }
    }

    #[test]
    fn provider_preserve_origin_keeps_unknown_valid_content_coding() {
        let mut headers = HeaderMap::new();
        headers.append(CONTENT_ENCODING, "gzip, X-Provider-Coding".parse().unwrap());
        headers.append(CONTENT_ENCODING, "compress".parse().unwrap());

        let selected = provider_response_headers(&headers, ProviderContentRepresentationMode::PreserveOrigin)
            .expect("valid provider headers");

        assert_eq!(selected_value(&selected, CONTENT_ENCODING.as_str()), Some("gzip, X-Provider-Coding, compress"));
    }

    #[test]
    fn provider_preserve_origin_rejects_malformed_or_non_text_content_encoding() {
        for value in [
            "gzip,,br".parse().unwrap(),
            reqwest::header::HeaderValue::from_bytes(&[0xff]).expect("opaque header value"),
        ] {
            let mut headers = HeaderMap::new();
            headers.insert(CONTENT_ENCODING, value);

            assert_eq!(
                provider_response_headers(&headers, ProviderContentRepresentationMode::PreserveOrigin),
                Err(ProviderResponseHeaderError::InvalidContentEncoding)
            );
        }
    }

    #[test]
    fn provider_preserve_origin_canonicalizes_identical_content_lengths() {
        let mut headers = HeaderMap::new();
        headers.append(CONTENT_LENGTH, "00321".parse().unwrap());
        headers.append(CONTENT_LENGTH, "321, 321".parse().unwrap());

        let selected = provider_response_headers(&headers, ProviderContentRepresentationMode::PreserveOrigin)
            .expect("matching content lengths");

        assert_eq!(selected_value(&selected, CONTENT_LENGTH.as_str()), Some("321"));
    }

    #[test]
    fn provider_preserve_origin_rejects_invalid_or_conflicting_content_lengths() {
        for values in [&["321", "322"][..], &["32x"][..], &[""][..]] {
            let mut headers = HeaderMap::new();
            for value in values {
                headers.append(CONTENT_LENGTH, (*value).parse().unwrap());
            }

            assert_eq!(
                provider_response_headers(&headers, ProviderContentRepresentationMode::PreserveOrigin),
                Err(ProviderResponseHeaderError::InvalidContentLength)
            );
        }

        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_LENGTH, reqwest::header::HeaderValue::from_bytes(&[0xff]).expect("opaque header value"));
        assert_eq!(
            provider_response_headers(&headers, ProviderContentRepresentationMode::PreserveOrigin),
            Err(ProviderResponseHeaderError::InvalidContentLength)
        );
    }

    #[test]
    fn provider_preserve_origin_rejects_ambiguous_or_non_text_content_range() {
        let mut headers = HeaderMap::new();
        headers.append(CONTENT_RANGE, "bytes 0-9/20".parse().unwrap());
        headers.append(CONTENT_RANGE, "bytes 10-19/20".parse().unwrap());
        assert_eq!(
            provider_response_headers(&headers, ProviderContentRepresentationMode::PreserveOrigin),
            Err(ProviderResponseHeaderError::InvalidContentRange)
        );

        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_RANGE, reqwest::header::HeaderValue::from_bytes(&[0xff]).expect("opaque header value"));
        assert_eq!(
            provider_response_headers(&headers, ProviderContentRepresentationMode::PreserveOrigin),
            Err(ProviderResponseHeaderError::InvalidContentRange)
        );

        for value in ["", "bytes 0-9/20, bytes 10-19/20"] {
            let mut headers = HeaderMap::new();
            headers.insert(CONTENT_RANGE, value.parse().unwrap());
            assert_eq!(
                provider_response_headers(&headers, ProviderContentRepresentationMode::PreserveOrigin),
                Err(ProviderResponseHeaderError::InvalidContentRange)
            );
        }
    }

    #[test]
    fn provider_identity_keeps_untransformed_content_length_and_content_range() {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_LENGTH, "10".parse().unwrap());
        headers.insert(CONTENT_RANGE, "bytes 10-19/100".parse().unwrap());
        headers.insert(TRANSFER_ENCODING, "chunked".parse().unwrap());

        let selected = provider_response_headers(&headers, ProviderContentRepresentationMode::Identity)
            .expect("valid untransformed identity headers");

        assert_eq!(selected_value(&selected, CONTENT_LENGTH.as_str()), Some("10"));
        assert_eq!(selected_value(&selected, CONTENT_RANGE.as_str()), Some("bytes 10-19/100"));
        assert_eq!(selected_value(&selected, CONTENT_ENCODING.as_str()), None);
        assert_eq!(selected_value(&selected, TRANSFER_ENCODING.as_str()), None);
    }

    #[test]
    fn provider_identity_rejects_invalid_untransformed_length_and_range() {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_LENGTH, "32x".parse().unwrap());
        assert_eq!(
            provider_response_headers(&headers, ProviderContentRepresentationMode::Identity),
            Err(ProviderResponseHeaderError::InvalidContentLength)
        );

        let mut headers = HeaderMap::new();
        headers.append(CONTENT_RANGE, "bytes 0-9/20".parse().unwrap());
        headers.append(CONTENT_RANGE, "bytes 10-19/20".parse().unwrap());
        assert_eq!(
            provider_response_headers(&headers, ProviderContentRepresentationMode::Identity),
            Err(ProviderResponseHeaderError::InvalidContentRange)
        );
    }

    #[test]
    fn provider_identity_uses_only_safe_normalized_headers() {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_ENCODING, "zstd".parse().unwrap());
        headers.insert(CONTENT_LENGTH, "99".parse().unwrap());
        headers.insert(CONTENT_RANGE, "bytes 0-98/99".parse().unwrap());
        headers.insert(ACCEPT_RANGES, "bytes".parse().unwrap());
        headers.insert(ETAG, "\"encoded\"".parse().unwrap());
        headers.insert(TRANSFER_ENCODING, "chunked".parse().unwrap());
        headers.insert(CONTENT_TYPE, "video/mp2t".parse().unwrap());
        headers.insert(CACHE_CONTROL, "public, max-age=30".parse().unwrap());
        headers.insert(SET_COOKIE, "provider_session=secret".parse().unwrap());
        headers.insert(PROXY_AUTHENTICATE, "Basic realm=provider".parse().unwrap());
        headers.insert(WWW_AUTHENTICATE, "Basic realm=provider".parse().unwrap());
        headers.insert("x-provider-secret", "secret".parse().unwrap());
        normalize_headers_after_content_decoding(&mut headers);

        let selected =
            provider_response_headers(&headers, ProviderContentRepresentationMode::for_playback_extension(".m3u8"))
                .expect("valid normalized provider headers");

        assert_eq!(selected_value(&selected, CONTENT_TYPE.as_str()), Some("video/mp2t"));
        assert_eq!(selected_value(&selected, CACHE_CONTROL.as_str()), Some("public, max-age=30"));
        for rejected in [
            CONTENT_ENCODING.as_str(),
            CONTENT_LENGTH.as_str(),
            CONTENT_RANGE.as_str(),
            ACCEPT_RANGES.as_str(),
            ETAG.as_str(),
            TRANSFER_ENCODING.as_str(),
            SET_COOKIE.as_str(),
            PROXY_AUTHENTICATE.as_str(),
            WWW_AUTHENTICATE.as_str(),
            "x-provider-secret",
        ] {
            assert_eq!(selected_value(&selected, rejected), None, "unexpected header {rejected}");
        }
    }

    #[test]
    fn rebuilt_stream_responses_never_forward_transfer_encoding() {
        let mut origin_headers = HeaderMap::new();
        origin_headers.insert(TRANSFER_ENCODING, "chunked".parse().unwrap());
        origin_headers.insert(CONTENT_TYPE, "video/mp2t".parse().unwrap());

        let selected = get_response_headers(&origin_headers);
        assert_eq!(selected_value(&selected, TRANSFER_ENCODING.as_str()), None);

        let (_, rebuilt) = get_stream_response_with_headers(Some((
            vec![
                (TRANSFER_ENCODING.as_str().to_owned(), "chunked".to_owned()),
                (CONTENT_TYPE.as_str().to_owned(), "video/mp2t".to_owned()),
            ],
            StatusCode::OK,
        )));
        assert!(!rebuilt.contains_key(TRANSFER_ENCODING));
        assert_eq!(rebuilt[CONTENT_TYPE], "video/mp2t");
    }
}
