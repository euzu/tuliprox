use super::DynReader;
use crate::utils::compression::compression_utils::{is_gzip, is_zlib_header};
use async_compression::tokio::bufread::{BrotliDecoder, DeflateDecoder, GzipDecoder, ZlibDecoder, ZstdDecoder};
use futures::TryStreamExt;
use reqwest::{
    header::{
        self, HeaderMap, HeaderName, HeaderValue, ACCEPT_RANGES, CONTENT_ENCODING, CONTENT_LENGTH, CONTENT_RANGE, ETAG,
        TRANSFER_ENCODING, VARY,
    },
    StatusCode,
};
use std::{
    error::Error as StdError,
    io,
    pin::Pin,
    task::{Context, Poll},
};
use tokio::io::{AsyncRead, AsyncReadExt, BufReader, ReadBuf};
use tokio_util::io::StreamReader;
use url::Url;

const MAX_MAGIC_PREFIX_LEN: usize = 4;
const ZSTD_MAGIC: [u8; 4] = [0x28, 0xb5, 0x2f, 0xfd];

/// Controls the final `Accept-Encoding` representation requested from an origin.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum OutboundContentCodingPolicy {
    #[default]
    Inherit,
    Identity,
}

/// Supported non-identity HTTP content codings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ContentCoding {
    Gzip,
    Deflate,
    Brotli,
    Zstd,
}

impl ContentCoding {
    /// Returns the fixed HTTP token used in redacted diagnostics.
    pub(crate) const fn as_http_token(self) -> &'static str {
        match self {
            Self::Gzip => "gzip",
            Self::Deflate => "deflate",
            Self::Brotli => "br",
            Self::Zstd => "zstd",
        }
    }
}

/// Safe fixed-field projection of an origin response whose coding was removed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ContentCodingObservation {
    pub(crate) content_encoding: &'static str,
    pub(crate) status: StatusCode,
    pub(crate) content_length: Option<u64>,
}

/// Selects the narrowly scoped fallback detection used when `Content-Encoding` is absent.
///
/// The shared `Declared` prefix deliberately makes header precedence explicit at every call site.
#[allow(clippy::enum_variant_names)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum ContentCodingDetection {
    /// Uses only declared `Content-Encoding` values.
    #[default]
    DeclaredOnly,
    /// Preserves the historical generic-text fallback for headerless gzip and zlib only.
    DeclaredOrLegacyTextMagic,
    /// Detects the known headerless gzip, zlib, and Zstandard signatures accepted for HLS manifests.
    DeclaredOrKnownHlsManifestMagic,
}

/// Origin response normalized to an asynchronous identity-representation body.
pub struct DecodedHttpResponse {
    pub(crate) status: StatusCode,
    pub(crate) final_url: Url,
    pub(crate) headers: HeaderMap,
    pub(crate) body: DynReader,
    decoded_from: Vec<ContentCoding>,
    original_content_length: Option<u64>,
}

impl DecodedHttpResponse {
    /// Reports whether at least one non-identity content coding was removed.
    pub(crate) fn was_content_decoded(&self) -> bool { !self.decoded_from.is_empty() }

    /// Returns only fixed and numeric fields suitable for HLS origin diagnostics.
    pub(crate) fn content_coding_observation(&self) -> Option<ContentCodingObservation> {
        let content_encoding = match self.decoded_from.as_slice() {
            [] => return None,
            [coding] => coding.as_http_token(),
            [_, ..] => "multiple",
        };
        Some(ContentCodingObservation {
            content_encoding,
            status: self.status,
            content_length: self.original_content_length,
        })
    }
}

/// Errors raised while parsing or preparing an HTTP content-coding chain.
#[derive(Debug, thiserror::Error)]
pub(crate) enum ContentCodingError {
    #[error("invalid Content-Encoding header")]
    InvalidHeader,

    #[error("unsupported Content-Encoding")]
    Unsupported(String),

    #[error("encoded partial content cannot be decoded safely")]
    EncodedPartialContent,

    #[error("failed to inspect encoded response prefix")]
    PrefixRead(#[from] io::Error),
}

/// Reports that a declared `Content-Encoding` field is not a valid HTTP token list.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("invalid Content-Encoding header")]
pub(crate) struct InvalidContentEncodingHeader;

/// Typed source attached to I/O failures emitted by a streaming decoder.
#[derive(Debug, thiserror::Error)]
#[error("content decoding failed: coding={coding:?}")]
pub(crate) struct ContentDecodingIoError {
    pub(crate) coding: ContentCoding,
}

/// Errors raised while fully consuming a decoded, size-bounded text body.
#[derive(Debug, thiserror::Error)]
pub(crate) enum ContentBodyReadError {
    #[error("decoded body exceeds configured limit {limit}")]
    LimitExceeded { limit: usize },

    #[error("decoded body is not valid UTF-8: valid_up_to={valid_up_to} error_len={error_len:?}")]
    InvalidUtf8 { valid_up_to: usize, error_len: Option<usize> },

    #[error(transparent)]
    Io(#[from] io::Error),
}

pub(crate) fn force_accept_encoding_identity(headers: &mut HeaderMap) {
    headers.insert(header::ACCEPT_ENCODING, HeaderValue::from_static("identity"));
}

pub(crate) fn apply_outbound_content_coding_policy(headers: &mut HeaderMap, policy: OutboundContentCodingPolicy) {
    if matches!(policy, OutboundContentCodingPolicy::Identity) {
        force_accept_encoding_identity(headers);
    }
}

/// Parses declared content-coding tokens without deciding whether Tuliprox can decode them.
pub(crate) fn parse_content_encoding_tokens(headers: &HeaderMap) -> Result<Vec<String>, InvalidContentEncodingHeader> {
    let mut tokens = Vec::new();
    for value in headers.get_all(CONTENT_ENCODING) {
        let value = value.to_str().map_err(|_| InvalidContentEncodingHeader)?;
        for token in value.split(',') {
            let token = token.trim_matches(|character| matches!(character, ' ' | '\t'));
            if !is_http_token(token) {
                return Err(InvalidContentEncodingHeader);
            }
            tokens.push(token.to_owned());
        }
    }
    Ok(tokens)
}

fn is_http_token(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(
                    byte,
                    b'!' | b'#'
                        | b'$'
                        | b'%'
                        | b'&'
                        | b'\''
                        | b'*'
                        | b'+'
                        | b'-'
                        | b'.'
                        | b'^'
                        | b'_'
                        | b'`'
                        | b'|'
                        | b'~'
                )
        })
}

pub(crate) fn parse_content_codings(headers: &HeaderMap) -> Result<Vec<ContentCoding>, ContentCodingError> {
    let mut codings = Vec::new();

    for token in parse_content_encoding_tokens(headers).map_err(|_| ContentCodingError::InvalidHeader)? {
        let coding = if token.eq_ignore_ascii_case("identity") {
            None
        } else if token.eq_ignore_ascii_case("gzip") || token.eq_ignore_ascii_case("x-gzip") {
            Some(ContentCoding::Gzip)
        } else if token.eq_ignore_ascii_case("deflate") {
            Some(ContentCoding::Deflate)
        } else if token.eq_ignore_ascii_case("br") {
            Some(ContentCoding::Brotli)
        } else if token.eq_ignore_ascii_case("zstd") {
            Some(ContentCoding::Zstd)
        } else {
            return Err(ContentCodingError::Unsupported(token));
        };

        if let Some(coding) = coding {
            codings.push(coding);
        }
    }

    Ok(codings)
}

pub(crate) async fn decode_response_to_identity(
    response: reqwest::Response,
    detection: ContentCodingDetection,
) -> Result<DecodedHttpResponse, ContentCodingError> {
    let status = response.status();
    let final_url = response.url().clone();
    let headers = response.headers().clone();
    let original_content_length =
        headers.get(CONTENT_LENGTH).and_then(|value| value.to_str().ok()).and_then(|value| value.parse::<u64>().ok());
    let has_content_encoding = headers.contains_key(CONTENT_ENCODING);
    let mut decoded_from = parse_content_codings(&headers)?;

    let stream = response.bytes_stream().map_err(|error| {
        let kind = if error.is_timeout() { io::ErrorKind::TimedOut } else { io::ErrorKind::Other };
        io::Error::new(kind, error)
    });
    let mut body: DynReader = Box::pin(StreamReader::new(stream));

    if !has_content_encoding && !matches!(detection, ContentCodingDetection::DeclaredOnly) {
        let (replayed_body, prefix) = inspect_prefix(body, MAX_MAGIC_PREFIX_LEN).await?;
        body = replayed_body;
        let detected = match detection {
            ContentCodingDetection::DeclaredOnly => None,
            ContentCodingDetection::DeclaredOrLegacyTextMagic => coding_from_legacy_text_magic(&prefix),
            ContentCodingDetection::DeclaredOrKnownHlsManifestMagic => coding_from_known_manifest_magic(&prefix),
        };
        if let Some(coding) = detected {
            decoded_from.push(coding);
        }
    }

    if status == StatusCode::PARTIAL_CONTENT && !decoded_from.is_empty() {
        return Err(ContentCodingError::EncodedPartialContent);
    }

    for coding in decoded_from.iter().rev().copied() {
        body = decoder_for(body, coding).await?;
    }

    let mut decoded = DecodedHttpResponse { status, final_url, headers, body, decoded_from, original_content_length };
    if decoded.was_content_decoded() {
        normalize_headers_after_content_decoding(&mut decoded.headers);
    } else if has_content_encoding {
        decoded.headers.remove(CONTENT_ENCODING);
    }

    Ok(decoded)
}

pub(crate) async fn read_to_end_limited<R>(reader: &mut R, max_bytes: usize) -> Result<Vec<u8>, ContentBodyReadError>
where
    R: AsyncRead + Unpin,
{
    let probe_limit = max_bytes.checked_add(1);
    let mut result = Vec::with_capacity(max_bytes.min(8 * 1024));
    let mut buffer = [0_u8; 8 * 1024];

    loop {
        let read_len = probe_limit.map_or(buffer.len(), |limit| limit.saturating_sub(result.len()).min(buffer.len()));
        if read_len == 0 {
            return Err(ContentBodyReadError::LimitExceeded { limit: max_bytes });
        }

        let read = reader.read(&mut buffer[..read_len]).await?;
        if read == 0 {
            return Ok(result);
        }
        result.extend_from_slice(&buffer[..read]);

        if result.len() > max_bytes {
            return Err(ContentBodyReadError::LimitExceeded { limit: max_bytes });
        }
    }
}

pub(crate) async fn read_utf8_limited<R>(reader: &mut R, max_bytes: usize) -> Result<String, ContentBodyReadError>
where
    R: AsyncRead + Unpin,
{
    let bytes = read_to_end_limited(reader, max_bytes).await?;
    String::from_utf8(bytes).map_err(|error| {
        let utf8_error = error.utf8_error();
        ContentBodyReadError::InvalidUtf8 { valid_up_to: utf8_error.valid_up_to(), error_len: utf8_error.error_len() }
    })
}

pub(crate) fn normalize_headers_after_content_decoding(headers: &mut HeaderMap) {
    for name in [
        CONTENT_ENCODING,
        CONTENT_LENGTH,
        CONTENT_RANGE,
        ACCEPT_RANGES,
        ETAG,
        HeaderName::from_static("content-md5"),
        HeaderName::from_static("digest"),
        HeaderName::from_static("content-digest"),
        HeaderName::from_static("repr-digest"),
        TRANSFER_ENCODING,
    ] {
        headers.remove(name);
    }

    remove_accept_encoding_from_vary(headers);
}

pub(crate) fn content_decoding_error_from_io(error: &io::Error) -> Option<&ContentDecodingIoError> {
    find_content_decoding_error(error.get_ref()?)
}

async fn decoder_for(mut reader: DynReader, coding: ContentCoding) -> Result<DynReader, ContentCodingError> {
    let reader = match coding {
        ContentCoding::Gzip => tagged_decoder(GzipDecoder::new(BufReader::new(reader)), coding),
        ContentCoding::Brotli => tagged_decoder(BrotliDecoder::new(BufReader::new(reader)), coding),
        ContentCoding::Zstd => tagged_decoder(ZstdDecoder::new(BufReader::new(reader)), coding),
        ContentCoding::Deflate => {
            let (replayed_reader, prefix) = inspect_prefix(reader, 2).await?;
            reader = replayed_reader;
            if is_zlib_header(&prefix) {
                tagged_decoder(ZlibDecoder::new(BufReader::new(reader)), coding)
            } else {
                tagged_decoder(DeflateDecoder::new(BufReader::new(reader)), coding)
            }
        }
    };
    Ok(reader)
}

fn tagged_decoder<R>(reader: R, coding: ContentCoding) -> DynReader
where
    R: AsyncRead + Send + Unpin + 'static,
{
    Box::pin(ContentDecodingReader { inner: reader, coding })
}

async fn inspect_prefix(mut reader: DynReader, max_len: usize) -> io::Result<(DynReader, Vec<u8>)> {
    let mut prefix = Vec::with_capacity(max_len);
    while prefix.len() < max_len {
        let mut chunk = [0_u8; MAX_MAGIC_PREFIX_LEN];
        let remaining = max_len - prefix.len();
        let read = reader.read(&mut chunk[..remaining]).await?;
        if read == 0 {
            break;
        }
        prefix.extend_from_slice(&chunk[..read]);
    }

    let replayed = std::io::Cursor::new(prefix.clone()).chain(reader);
    Ok((Box::pin(replayed), prefix))
}

fn coding_from_known_manifest_magic(prefix: &[u8]) -> Option<ContentCoding> {
    coding_from_legacy_text_magic(prefix).or_else(|| prefix.starts_with(&ZSTD_MAGIC).then_some(ContentCoding::Zstd))
}

fn coding_from_legacy_text_magic(prefix: &[u8]) -> Option<ContentCoding> {
    if is_gzip(prefix) {
        Some(ContentCoding::Gzip)
    } else {
        is_zlib_header(prefix).then_some(ContentCoding::Deflate)
    }
}

fn remove_accept_encoding_from_vary(headers: &mut HeaderMap) {
    let values = headers.get_all(VARY).iter().cloned().collect::<Vec<_>>();
    if values.is_empty() {
        return;
    }
    headers.remove(VARY);

    for value in values {
        let original_value = value.clone();
        let Ok(value) = value.to_str() else {
            headers.append(VARY, original_value);
            continue;
        };
        let remaining = value
            .split(',')
            .map(str::trim)
            .filter(|token| !token.eq_ignore_ascii_case("accept-encoding"))
            .collect::<Vec<_>>();
        if !remaining.is_empty() {
            match HeaderValue::from_str(&remaining.join(", ")) {
                Ok(value) => {
                    headers.append(VARY, value);
                }
                Err(_) => {
                    headers.append(VARY, original_value);
                }
            }
        }
    }
}

fn find_content_decoding_error<'a>(mut error: &'a (dyn StdError + 'static)) -> Option<&'a ContentDecodingIoError> {
    loop {
        if let Some(error) = error.downcast_ref::<ContentDecodingIoError>() {
            return Some(error);
        }
        error = error.source()?;
    }
}

struct ContentDecodingReader<R> {
    inner: R,
    coding: ContentCoding,
}

impl<R> AsyncRead for ContentDecodingReader<R>
where
    R: AsyncRead + Unpin,
{
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match Pin::new(&mut self.inner).poll_read(context, buffer) {
            Poll::Ready(Err(error))
                if content_decoding_error_from_io(&error).is_none() && !is_http_body_transport_error(&error) =>
            {
                let kind = error.kind();
                Poll::Ready(Err(io::Error::new(kind, ContentDecodingIoError { coding: self.coding })))
            }
            result => result,
        }
    }
}

pub(crate) fn is_http_body_transport_error(error: &io::Error) -> bool {
    let Some(source) = error.get_ref() else {
        return false;
    };
    let mut source: &(dyn StdError + 'static) = source;
    loop {
        if source.downcast_ref::<reqwest::Error>().is_some() {
            return true;
        }
        let Some(next) = source.source() else {
            return false;
        };
        source = next;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_compression::tokio::bufread::{BrotliEncoder, DeflateEncoder, GzipEncoder, ZlibEncoder, ZstdEncoder};
    use reqwest::redirect::Policy;
    use std::io::Cursor;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    const MANIFEST: &[u8] = b"#EXTM3U\n#EXT-X-VERSION:3\nsegment.ts\n";

    #[test]
    fn content_coding_parses_all_headers_tokens_aliases_and_identity() {
        let mut headers = HeaderMap::new();
        headers.append(CONTENT_ENCODING, HeaderValue::from_static("gzip, identity"));
        headers.append(CONTENT_ENCODING, HeaderValue::from_static("BR, x-GZiP, deflate, zstd"));

        assert_eq!(
            parse_content_codings(&headers).expect("valid codings"),
            vec![
                ContentCoding::Gzip,
                ContentCoding::Brotli,
                ContentCoding::Gzip,
                ContentCoding::Deflate,
                ContentCoding::Zstd,
            ]
        );
    }

    #[test]
    fn content_coding_rejects_empty_invalid_and_unsupported_tokens() {
        for value in [HeaderValue::from_static("gzip,"), HeaderValue::from_static("gzip; level=1")] {
            let mut headers = HeaderMap::new();
            headers.insert(CONTENT_ENCODING, value);
            assert!(matches!(parse_content_codings(&headers), Err(ContentCodingError::InvalidHeader)));
        }

        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_ENCODING, HeaderValue::from_bytes(&[0xff]).expect("opaque header value"));
        assert!(matches!(parse_content_codings(&headers), Err(ContentCodingError::InvalidHeader)));

        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_ENCODING, HeaderValue::from_static("compress"));
        assert!(matches!(
            parse_content_codings(&headers),
            Err(ContentCodingError::Unsupported(coding)) if coding == "compress"
        ));
    }

    #[test]
    fn content_encoding_token_parser_preserves_unknown_valid_tokens_and_order() {
        let mut headers = HeaderMap::new();
        headers.append(CONTENT_ENCODING, HeaderValue::from_static("gzip, X-Provider-Coding"));
        headers.append(CONTENT_ENCODING, HeaderValue::from_static("compress"));

        assert_eq!(
            parse_content_encoding_tokens(&headers).expect("valid HTTP tokens"),
            vec!["gzip".to_owned(), "X-Provider-Coding".to_owned(), "compress".to_owned()]
        );
        assert!(matches!(
            parse_content_codings(&headers),
            Err(ContentCodingError::Unsupported(coding)) if coding == "X-Provider-Coding"
        ));
    }

    #[test]
    fn content_coding_identity_policy_overrides_existing_value_only_when_requested() {
        let mut headers = HeaderMap::new();
        headers.insert(header::ACCEPT_ENCODING, HeaderValue::from_static("gzip, br"));

        apply_outbound_content_coding_policy(&mut headers, OutboundContentCodingPolicy::Inherit);
        assert_eq!(headers[header::ACCEPT_ENCODING], "gzip, br");

        apply_outbound_content_coding_policy(&mut headers, OutboundContentCodingPolicy::Identity);
        assert_eq!(headers[header::ACCEPT_ENCODING], "identity");
    }

    #[tokio::test]
    async fn content_coding_decodes_all_declared_codings_and_deflate_wrappers() {
        let cases = [
            ("gzip", Encoding::Gzip),
            ("x-gzip", Encoding::Gzip),
            ("deflate", Encoding::Zlib),
            ("deflate", Encoding::RawDeflate),
            ("br", Encoding::Brotli),
            ("zstd", Encoding::Zstd),
        ];

        for (header_value, encoding) in cases {
            let encoded = encode(MANIFEST, encoding).await;
            let response = local_response(StatusCode::OK, &[("Content-Encoding", header_value)], encoded).await;
            let mut decoded = decode_response_to_identity(response, ContentCodingDetection::DeclaredOnly)
                .await
                .expect("decoder setup");

            assert_eq!(
                read_to_end_limited(&mut decoded.body, 1024).await.expect("decoded body"),
                MANIFEST,
                "failed for {header_value} / {encoding:?}"
            );
            assert!(!decoded.headers.contains_key(CONTENT_ENCODING));
        }
    }

    #[tokio::test]
    async fn content_coding_decodes_multiple_header_lines_in_reverse_application_order() {
        let gzip = encode(MANIFEST, Encoding::Gzip).await;
        let brotli_over_gzip = encode(&gzip, Encoding::Brotli).await;
        let encoded_length = brotli_over_gzip.len() as u64;
        let response = local_response(
            StatusCode::OK,
            &[("Content-Encoding", "gzip"), ("Content-Encoding", "br")],
            brotli_over_gzip,
        )
        .await;

        let mut decoded =
            decode_response_to_identity(response, ContentCodingDetection::DeclaredOnly).await.expect("decoder setup");
        assert_eq!(decoded.decoded_from, vec![ContentCoding::Gzip, ContentCoding::Brotli]);
        assert_eq!(
            decoded.content_coding_observation(),
            Some(ContentCodingObservation {
                content_encoding: "multiple",
                status: StatusCode::OK,
                content_length: Some(encoded_length),
            })
        );
        assert_eq!(read_to_end_limited(&mut decoded.body, 1024).await.expect("decoded body"), MANIFEST);
    }

    #[tokio::test]
    async fn content_coding_decodes_comma_separated_codings_in_reverse_application_order() {
        let gzip = encode(MANIFEST, Encoding::Gzip).await;
        let brotli_over_gzip = encode(&gzip, Encoding::Brotli).await;
        let response = local_response(StatusCode::OK, &[("Content-Encoding", "gzip, br")], brotli_over_gzip).await;

        let mut decoded =
            decode_response_to_identity(response, ContentCodingDetection::DeclaredOnly).await.expect("decoder setup");
        assert_eq!(decoded.decoded_from, vec![ContentCoding::Gzip, ContentCoding::Brotli]);
        assert_eq!(read_to_end_limited(&mut decoded.body, 1024).await.expect("decoded body"), MANIFEST);
    }

    #[tokio::test]
    async fn content_coding_declared_value_wins_over_contradicting_magic() {
        for detection in
            [ContentCodingDetection::DeclaredOrLegacyTextMagic, ContentCodingDetection::DeclaredOrKnownHlsManifestMagic]
        {
            let gzip = encode(MANIFEST, Encoding::Gzip).await;
            let response = local_response(StatusCode::OK, &[("Content-Encoding", "br")], gzip).await;
            let mut decoded = decode_response_to_identity(response, detection).await.expect("lazy decoder setup");

            let error = read_to_end_limited(&mut decoded.body, 1024)
                .await
                .expect_err("gzip bytes must not override declared Brotli");
            let ContentBodyReadError::Io(error) = error else {
                panic!("expected decoder I/O error");
            };
            assert_eq!(
                content_decoding_error_from_io(&error).expect("typed decoder error").coding,
                ContentCoding::Brotli
            );
        }
    }

    #[tokio::test]
    async fn content_coding_legacy_text_magic_decodes_only_headerless_gzip_and_zlib() {
        for encoding in [Encoding::Gzip, Encoding::Zlib] {
            let encoded = encode(MANIFEST, encoding).await;
            let response = local_response(StatusCode::OK, &[], encoded).await;
            let mut decoded = decode_response_to_identity(response, ContentCodingDetection::DeclaredOrLegacyTextMagic)
                .await
                .expect("decoder setup");

            assert_eq!(
                read_to_end_limited(&mut decoded.body, 1024).await.expect("magic-decoded body"),
                MANIFEST,
                "failed for {encoding:?}"
            );
            assert!(decoded.was_content_decoded());
        }

        for encoding in [Encoding::RawDeflate, Encoding::Brotli, Encoding::Zstd] {
            let encoded = encode(MANIFEST, encoding).await;
            let response = local_response(StatusCode::OK, &[], encoded.clone()).await;
            let mut untouched =
                decode_response_to_identity(response, ContentCodingDetection::DeclaredOrLegacyTextMagic)
                    .await
                    .expect("identity setup");

            assert!(!untouched.was_content_decoded(), "must not guess {encoding:?}");
            assert_eq!(read_to_end_limited(&mut untouched.body, 1024).await.expect("untouched body"), encoded);
        }
    }

    #[tokio::test]
    async fn content_coding_hls_manifest_magic_supports_only_known_signatures() {
        for encoding in [Encoding::Gzip, Encoding::Zlib, Encoding::Zstd] {
            let encoded = encode(MANIFEST, encoding).await;
            let response = local_response(StatusCode::OK, &[], encoded).await;
            let mut decoded =
                decode_response_to_identity(response, ContentCodingDetection::DeclaredOrKnownHlsManifestMagic)
                    .await
                    .expect("decoder setup");
            assert_eq!(
                read_to_end_limited(&mut decoded.body, 1024).await.expect("magic-decoded body"),
                MANIFEST,
                "failed for {encoding:?}"
            );
        }

        let encoded_binary = encode(b"not a manifest", Encoding::Gzip).await;
        let response = local_response(StatusCode::OK, &[], encoded_binary.clone()).await;
        let mut untouched =
            decode_response_to_identity(response, ContentCodingDetection::DeclaredOnly).await.expect("identity setup");
        assert!(untouched.decoded_from.is_empty());
        assert_eq!(read_to_end_limited(&mut untouched.body, 1024).await.expect("untouched body"), encoded_binary);
    }

    #[tokio::test]
    async fn content_coding_header_presence_disables_manifest_magic_even_for_identity() {
        let encoded = encode(MANIFEST, Encoding::Gzip).await;
        let response = local_response(StatusCode::OK, &[("Content-Encoding", "identity")], encoded.clone()).await;
        let mut decoded =
            decode_response_to_identity(response, ContentCodingDetection::DeclaredOrKnownHlsManifestMagic)
                .await
                .expect("identity setup");

        assert!(decoded.decoded_from.is_empty());
        assert_eq!(decoded.content_coding_observation(), None);
        assert!(!decoded.headers.contains_key(CONTENT_ENCODING));
        assert_eq!(read_to_end_limited(&mut decoded.body, 1024).await.expect("identity body"), encoded);
    }

    #[tokio::test]
    async fn content_coding_rejects_encoded_partial_content_but_preserves_identity_range_headers() {
        let encoded = encode(MANIFEST, Encoding::Gzip).await;
        let response = local_response(
            StatusCode::PARTIAL_CONTENT,
            &[("Content-Encoding", "gzip"), ("Content-Range", "bytes 0-9/20")],
            encoded,
        )
        .await;
        assert!(matches!(
            decode_response_to_identity(response, ContentCodingDetection::DeclaredOnly).await,
            Err(ContentCodingError::EncodedPartialContent)
        ));

        let response = local_response(
            StatusCode::PARTIAL_CONTENT,
            &[("Content-Encoding", "identity"), ("Content-Range", "bytes 0-2/10")],
            b"abc".to_vec(),
        )
        .await;
        let decoded = decode_response_to_identity(response, ContentCodingDetection::DeclaredOnly)
            .await
            .expect("identity partial response");
        assert_eq!(decoded.status, StatusCode::PARTIAL_CONTENT);
        assert_eq!(decoded.headers[CONTENT_RANGE], "bytes 0-2/10");
    }

    #[tokio::test]
    async fn content_coding_normalizes_only_representation_dependent_headers() {
        let encoded = encode(MANIFEST, Encoding::Gzip).await;
        let response = local_response(
            StatusCode::OK,
            &[
                ("Content-Encoding", "gzip"),
                ("Content-Range", "bytes 0-9/10"),
                ("Accept-Ranges", "bytes"),
                ("ETag", "origin-tag"),
                ("Content-MD5", "abc"),
                ("Digest", "sha-256=abc"),
                ("Content-Digest", "sha-256=:abc:"),
                ("Repr-Digest", "sha-256=:abc:"),
                ("Vary", "Accept-Encoding, Origin"),
                ("Vary", "*"),
                ("Set-Cookie", "session=secret"),
            ],
            encoded,
        )
        .await;
        let decoded = decode_response_to_identity(response, ContentCodingDetection::DeclaredOnly)
            .await
            .expect("decoded response");

        for name in [
            CONTENT_ENCODING,
            CONTENT_LENGTH,
            CONTENT_RANGE,
            ACCEPT_RANGES,
            ETAG,
            HeaderName::from_static("content-md5"),
            HeaderName::from_static("digest"),
            HeaderName::from_static("content-digest"),
            HeaderName::from_static("repr-digest"),
            TRANSFER_ENCODING,
        ] {
            assert!(!decoded.headers.contains_key(&name), "stale header {name}");
        }
        let vary =
            decoded.headers.get_all(VARY).iter().map(|value| value.to_str().expect("ASCII Vary")).collect::<Vec<_>>();
        assert_eq!(vary, vec!["Origin", "*"]);
        assert_eq!(decoded.headers[header::SET_COOKIE], "session=secret");
    }

    #[test]
    fn content_coding_normalizer_removes_transfer_encoding_but_keeps_safe_metadata() {
        let mut headers = HeaderMap::new();
        headers.insert(TRANSFER_ENCODING, HeaderValue::from_static("chunked"));
        headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/vnd.apple.mpegurl"));

        normalize_headers_after_content_decoding(&mut headers);

        assert!(!headers.contains_key(TRANSFER_ENCODING));
        assert_eq!(headers[header::CONTENT_TYPE], "application/vnd.apple.mpegurl");
    }

    #[test]
    fn content_coding_normalizer_preserves_opaque_vary_without_panicking() {
        let opaque_vary = HeaderValue::from_bytes(&[0xff]).expect("opaque header value");
        let mut headers = HeaderMap::new();
        headers.insert(VARY, opaque_vary.clone());

        normalize_headers_after_content_decoding(&mut headers);

        assert_eq!(headers[VARY], opaque_vary);
    }

    #[tokio::test]
    async fn content_coding_surfaces_typed_errors_for_truncated_streams() {
        for (encoding, coding) in [
            (Encoding::Gzip, ContentCoding::Gzip),
            (Encoding::Brotli, ContentCoding::Brotli),
            (Encoding::Zstd, ContentCoding::Zstd),
        ] {
            let mut encoded = encode(MANIFEST, encoding).await;
            encoded.truncate(encoded.len() / 2);
            let response =
                local_response(StatusCode::OK, &[("Content-Encoding", encoding.header_value())], encoded).await;
            let mut decoded = decode_response_to_identity(response, ContentCodingDetection::DeclaredOnly)
                .await
                .expect("lazy decoder setup");

            let error = read_to_end_limited(&mut decoded.body, 1024).await.expect_err("truncated stream");
            let ContentBodyReadError::Io(error) = error else {
                panic!("expected decoder I/O error for {encoding:?}");
            };
            assert_eq!(content_decoding_error_from_io(&error).expect("typed decoder error").coding, coding);
        }
    }

    #[tokio::test]
    async fn content_coding_limit_and_utf8_errors_are_distinct() {
        let mut exact = Cursor::new(b"abcd".to_vec());
        assert_eq!(read_to_end_limited(&mut exact, 4).await.expect("exact limit"), b"abcd");

        let mut oversized = Cursor::new(b"abcde".to_vec());
        assert!(matches!(
            read_to_end_limited(&mut oversized, 4).await,
            Err(ContentBodyReadError::LimitExceeded { limit: 4 })
        ));

        let mut invalid_utf8 = Cursor::new(vec![b'a', 0xff]);
        assert!(matches!(
            read_utf8_limited(&mut invalid_utf8, 10).await,
            Err(ContentBodyReadError::InvalidUtf8 { valid_up_to: 1, error_len: Some(1) })
        ));
    }

    #[derive(Debug, Clone, Copy)]
    enum Encoding {
        Gzip,
        Zlib,
        RawDeflate,
        Brotli,
        Zstd,
    }

    impl Encoding {
        const fn header_value(self) -> &'static str {
            match self {
                Self::Gzip => "gzip",
                Self::Zlib | Self::RawDeflate => "deflate",
                Self::Brotli => "br",
                Self::Zstd => "zstd",
            }
        }
    }

    async fn encode(input: &[u8], encoding: Encoding) -> Vec<u8> {
        let reader = BufReader::new(Cursor::new(input.to_vec()));
        let mut result = Vec::new();
        match encoding {
            Encoding::Gzip => GzipEncoder::new(reader).read_to_end(&mut result).await.expect("gzip encode"),
            Encoding::Zlib => ZlibEncoder::new(reader).read_to_end(&mut result).await.expect("zlib encode"),
            Encoding::RawDeflate => {
                DeflateEncoder::new(reader).read_to_end(&mut result).await.expect("raw deflate encode")
            }
            Encoding::Brotli => BrotliEncoder::new(reader).read_to_end(&mut result).await.expect("Brotli encode"),
            Encoding::Zstd => ZstdEncoder::new(reader).read_to_end(&mut result).await.expect("Zstandard encode"),
        };
        result
    }

    async fn local_response(status: StatusCode, headers: &[(&str, &str)], body: Vec<u8>) -> reqwest::Response {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind test origin");
        let address = listener.local_addr().expect("test origin address");
        let has_content_length = headers.iter().any(|(name, _)| name.eq_ignore_ascii_case("content-length"));
        let owned_headers =
            headers.iter().map(|(name, value)| ((*name).to_owned(), (*value).to_owned())).collect::<Vec<_>>();

        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept test request");
            let mut request = Vec::new();
            let mut buffer = [0_u8; 1024];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let read = socket.read(&mut buffer).await.expect("read test request");
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..read]);
            }

            let mut response = format!("HTTP/1.1 {} Test\r\nConnection: close\r\n", status.as_u16());
            if !has_content_length {
                response.push_str("Content-Length: ");
                response.push_str(&body.len().to_string());
                response.push_str("\r\n");
            }
            for (name, value) in owned_headers {
                response.push_str(&name);
                response.push_str(": ");
                response.push_str(&value);
                response.push_str("\r\n");
            }
            response.push_str("\r\n");
            socket.write_all(response.as_bytes()).await.expect("write test response head");
            socket.write_all(&body).await.expect("write test response body");
        });

        reqwest::Client::builder()
            .no_proxy()
            .redirect(Policy::none())
            .build()
            .expect("test client")
            .get(format!("http://{address}/manifest.m3u8"))
            .send()
            .await
            .expect("fetch test response")
    }
}
