use crate::stalker::error::{StalkerError, StalkerResult};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use url::Url;

/// A Stalker `cmd` field is usually a plain-text space-prefixed command + URL pair, e.g.
/// `"ffmpeg http://portal.example/stream/123"` or a bare URL. Some portals embed a
/// placeholder command like `"auto"` or `"localhost"` instead of `ffmpeg`, and a few
/// legacy middlewares base64-encode the whole pair. The helpers below recover the
/// underlying URL — plain text first, base64 as a fallback — and validate that the
/// scheme is one we can reverse-proxy.
pub fn extract_url_from_cmd(raw_cmd: &str) -> StalkerResult<String> {
    let trimmed = raw_cmd.trim();
    if let Some(result) = extract_plain(trimmed) {
        return result;
    }
    let bytes = BASE64
        .decode(trimmed.as_bytes())
        .map_err(|err| StalkerError::BodyDecode { message: format!("cmd base64 decode failed: {err}") })?;
    let decoded = String::from_utf8(bytes)
        .map_err(|err| StalkerError::BodyDecode { message: format!("cmd utf-8 decode failed: {err}") })?;
    extract_plain(decoded.trim())
        .unwrap_or_else(|| Err(StalkerError::BodyDecode { message: "cmd contains no parseable url".to_string() }))
}

/// Try to recover a URL from a plain-text cmd. Returns `None` when no URL-shaped token is
/// present (the caller then falls back to base64 decoding); returns
/// `Some(Err(UnsupportedScheme))` when a URL parses but uses a scheme we cannot proxy.
fn extract_plain(cmd: &str) -> Option<StalkerResult<String>> {
    let whole = cmd.trim();
    if let Ok(url) = Url::parse(whole) {
        return Some(check_playable(whole, &url));
    }
    let (_, rest) = whole.split_once(' ')?;
    let candidate = rest.trim();
    let url = Url::parse(candidate).ok()?;
    Some(check_playable(candidate, &url))
}

fn check_playable(candidate: &str, url: &Url) -> StalkerResult<String> {
    if scheme_is_playable(url.scheme()) {
        Ok(candidate.to_string())
    } else {
        Err(StalkerError::UnsupportedScheme { scheme: url.scheme().to_string() })
    }
}

/// The supported playable schemes — anything else is rejected at `create_link` time.
///
/// The current policy accepts **http** and **https** only. Stalker portals
/// occasionally emit `rtmp://` and `rtsp://` cmd payloads (legacy MAG devices
/// rely on ffmpeg wrappers for those), but the reverse proxy in tuliprox can
/// only relay HTTP-family traffic to its clients. Rejecting non-HTTP schemes
/// up-front keeps the supported-transport policy testable and prevents
/// silently broken streams from leaking into M3U/xtream outputs.
pub fn scheme_is_playable(scheme: &str) -> bool {
    matches!(scheme.to_ascii_lowercase().as_str(), "http" | "https")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b64(s: &str) -> String {
        BASE64.encode(s.as_bytes())
    }

    #[test]
    fn extracts_url_after_space() {
        let cmd = b64("ffmpeg http://portal.example/stream/123");
        let url = extract_url_from_cmd(&cmd).expect("decode should succeed");
        assert_eq!(url, "http://portal.example/stream/123");
    }

    #[test]
    fn extracts_plain_text_ffmpeg_cmd() {
        let url = extract_url_from_cmd("ffmpeg http://line.example/live.ts").expect("plain cmd");
        assert_eq!(url, "http://line.example/live.ts");
    }

    #[test]
    fn extracts_plain_text_auto_cmd() {
        let url = extract_url_from_cmd("auto http://portal.example/stream/9").expect("plain cmd");
        assert_eq!(url, "http://portal.example/stream/9");
    }

    #[test]
    fn extracts_bare_url_cmd() {
        let url = extract_url_from_cmd("https://portal.example/stream/77?token=x").expect("bare url");
        assert_eq!(url, "https://portal.example/stream/77?token=x");
    }

    #[test]
    fn rejects_plain_cmd_with_unplayable_scheme() {
        let err = extract_url_from_cmd("ffmpeg rtmp://portal.example/live").expect_err("should fail");
        assert!(matches!(err, StalkerError::UnsupportedScheme { .. }));
    }

    #[test]
    fn rejects_base64_cmd_with_unplayable_scheme() {
        let cmd = b64("ffmpeg file:///etc/passwd");
        let err = extract_url_from_cmd(&cmd).expect_err("should fail");
        assert!(matches!(err, StalkerError::UnsupportedScheme { .. }));
    }

    #[test]
    fn rejects_invalid_base64() {
        let err = extract_url_from_cmd("!!!not_base64!!!").expect_err("should fail");
        assert!(matches!(err, StalkerError::BodyDecode { .. }));
    }

    #[test]
    fn rejects_non_url_payload() {
        let cmd = b64("ffmpeg not a url");
        let err = extract_url_from_cmd(&cmd).expect_err("should fail");
        assert!(matches!(err, StalkerError::BodyDecode { .. }));
    }

    #[test]
    fn scheme_check_is_case_insensitive() {
        assert!(scheme_is_playable("HTTP"));
        assert!(!scheme_is_playable("rtsP"));
        assert!(!scheme_is_playable("file"));
        assert!(!scheme_is_playable(""));
    }

    #[test]
    fn rejects_rtmp_scheme() {
        // rtmp:// is a valid Stalker cmd payload but the reverse proxy cannot relay
        // it to a regular HTTP client. Explicit rejection keeps the supported scheme
        // policy testable instead of letting unsupported streams through silently.
        assert!(!scheme_is_playable("rtmp"));
        assert!(!scheme_is_playable("RTMP"));
    }

    #[test]
    fn rejects_rtsp_scheme() {
        assert!(!scheme_is_playable("rtsp"));
        assert!(!scheme_is_playable("RTSP"));
    }

    #[test]
    fn accepts_http_and_https() {
        assert!(scheme_is_playable("http"));
        assert!(scheme_is_playable("https"));
        assert!(scheme_is_playable("HTTP"));
        assert!(scheme_is_playable("HTTPS"));
    }
}
