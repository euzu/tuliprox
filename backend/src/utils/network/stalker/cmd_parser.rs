use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use url::Url;

use crate::utils::network::stalker::error::{StalkerError, StalkerResult};

/// A Stalker `cmd` field is a space-prefixed command + base64-encoded URL pair, e.g.
/// `"ffmpeg http://portal.example/stream/123"`. Some portals embed a placeholder command
/// like `"auto"` or `"localhost"` instead of `ffmpeg`. The helpers below recover the
/// underlying URL and validate that the scheme is one we can reverse-proxy.
pub fn extract_url_from_cmd(raw_cmd: &str) -> StalkerResult<String> {
    let trimmed = raw_cmd.trim();
    let bytes = BASE64
        .decode(trimmed.as_bytes())
        .map_err(|err| StalkerError::BodyDecode {
            message: format!("cmd base64 decode failed: {err}"),
        })?;
    let decoded = String::from_utf8(bytes).map_err(|err| StalkerError::BodyDecode {
        message: format!("cmd utf-8 decode failed: {err}"),
    })?;
    let after_space = decoded.split_once(' ').map_or(decoded.as_str(), |(_, rest)| rest);
    let candidate = after_space.trim();
    Url::parse(candidate).map_err(|err| StalkerError::BodyDecode {
        message: format!("cmd url parse failed: {err}"),
    })?;
    Ok(candidate.to_string())
}

/// Probe whether a raw `cmd` decodes cleanly. Used to skip URLs that the portal will not
/// be able to play (placeholder commands, broken base64, etc.).
pub fn cmd_is_decodable(raw_cmd: &str) -> bool { extract_url_from_cmd(raw_cmd).is_ok() }

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

    fn b64(s: &str) -> String { BASE64.encode(s.as_bytes()) }

    #[test]
    fn extracts_url_after_space() {
        let cmd = b64("ffmpeg http://portal.example/stream/123");
        let url = extract_url_from_cmd(&cmd).expect("decode should succeed");
        assert_eq!(url, "http://portal.example/stream/123");
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
