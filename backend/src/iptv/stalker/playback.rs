use log::warn;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use shared::model::stalker::{StalkerPlaybackMode, StalkerStreamKind};
use url::Url;

use crate::iptv::stalker::client::{validate_playable_scheme, StalkerApiClient};
use crate::iptv::stalker::cmd_parser::scheme_is_playable;
use crate::iptv::stalker::error::{safe_stalker_url, StalkerError, StalkerResult};
use crate::iptv::stalker::profile::{StalkerHandshake, StalkerResolvedStream};
use crate::iptv::stalker::recipes::recipe_spec_for;

#[derive(Debug, Default, Clone, Deserialize, Serialize)]
struct StalkerCreateLinkResponse {
    #[serde(default)]
    js: Option<Value>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    cmd: Option<String>,
}

/// Resolve a `cmd` to a playable URL. The portal is told which kind of stream we want
/// (`live`, `movie`, `episode` or `archive`) and the original `cmd` field. The response is
/// expected to be a JSON object whose `js.cmd` field is a base64-encoded `ffmpeg <url>`
/// pair, with the URL extracted by [`crate::iptv::stalker::cmd_parser`].
///
/// `requested_mode` is the playback mode the caller already decided on (from the
/// item's temp-link capability flags). The resolved stream preserves this mode so
/// downstream layers (reverse-proxy, headers) can adapt to nginx-secure / flussonic /
/// wowza without re-deriving it.
#[allow(clippy::too_many_arguments)]
pub async fn create_link(
    client: &StalkerApiClient,
    handshake: &StalkerHandshake,
    kind: StalkerStreamKind,
    requested_mode: StalkerPlaybackMode,
    cmd: &str,
    series_number: Option<u32>,
    archive_start: Option<&str>,
    archive_end: Option<&str>,
) -> StalkerResult<StalkerResolvedStream> {
    let spec = recipe_spec_for(handshake.profile.bootstrap_recipe);
    let candidates = client.load_url_candidates().to_vec();
    let mut last_err: Option<StalkerError> = None;
    for load_url in candidates {
        let builder = build_create_link_builder(client, &load_url, handshake, &spec, kind, cmd, series_number, archive_start, archive_end);
        match client.send_json::<StalkerCreateLinkResponse>(builder, "create_link").await {
            Ok(resp) => {
                return resolve_response(resp, kind, requested_mode, cmd);
            }
            Err(err) => {
                last_err = Some(err);
            }
        }
    }
    Err(last_err.unwrap_or_else(|| StalkerError::NoEndpoint { portal: safe_stalker_url(client.portal_url()) }))
}

#[allow(clippy::too_many_arguments)]
fn build_create_link_builder(
    client: &StalkerApiClient,
    load_url: &crate::iptv::stalker::url_factory::StalkerLoadUrl,
    handshake: &StalkerHandshake,
    spec: &crate::iptv::stalker::recipes::StalkerRecipeSpec,
    kind: StalkerStreamKind,
    cmd: &str,
    series_number: Option<u32>,
    archive_start: Option<&str>,
    archive_end: Option<&str>,
) -> reqwest::RequestBuilder {
    // Build query parameters as a flat (key, value) pair list. We use String values so
    // we can defer the borrow until the builder consumes them — reqwest's `.query`
    // accepts any `Serialize` so a `Vec<(String, String)>` is enough.
    let portal_type = match kind {
        StalkerStreamKind::Live | StalkerStreamKind::Archive => "itv",
        StalkerStreamKind::Movie | StalkerStreamKind::Episode => "vod",
    };
    let forced_storage = match kind {
        StalkerStreamKind::Live | StalkerStreamKind::Archive => "undefined",
        StalkerStreamKind::Movie | StalkerStreamKind::Episode => "0",
    };
    let mut query_pairs: Vec<(String, String)> = vec![
        ("type".to_string(), portal_type.to_string()),
        ("action".to_string(), "create_link".to_string()),
        ("JsHttpRequest".to_string(), "1-xml".to_string()),
        ("cmd".to_string(), cmd.to_string()),
        ("series".to_string(), series_number.unwrap_or(0).to_string()),
        ("forced_storage".to_string(), forced_storage.to_string()),
        ("disable_ad".to_string(), "0".to_string()),
        ("download".to_string(), "0".to_string()),
    ];
    if matches!(kind, StalkerStreamKind::Archive) {
        if let Some(start) = archive_start {
            query_pairs.push(("start".to_string(), start.to_string()));
        }
        if let Some(end) = archive_end {
            query_pairs.push(("end".to_string(), end.to_string()));
        }
    }
    let mut builder = client
        .http()
        .get(&load_url.load_url)
        .headers(client.common_headers(load_url))
        .query(&query_pairs);
    builder = client.apply_mac_query(builder);
    builder = client.apply_bearer(builder, Some(&handshake.session), spec.token_in_query);
    builder
}

fn resolve_response(
    resp: StalkerCreateLinkResponse,
    kind: StalkerStreamKind,
    requested_mode: StalkerPlaybackMode,
    source_cmd: &str,
) -> StalkerResult<StalkerResolvedStream> {
    if let Some(err) = &resp.error {
        if !err.is_empty() {
            warn!("Stalker create_link error: {err}");
            return Err(StalkerError::PortalRefusedCmd { reason: err.clone() });
        }
    }
    let raw_cmd = resp
        .js
        .as_ref()
        .and_then(|js| js.get("cmd").and_then(Value::as_str).map(String::from))
        .or_else(|| resp.text.as_ref().and_then(|t| serde_json::from_str::<Value>(t).ok().and_then(|v| v.get("cmd").and_then(Value::as_str).map(String::from))))
        .or(resp.cmd);
    let Some(raw_cmd) = raw_cmd else {
        return Err(StalkerError::PortalRefusedCmd {
            reason: "create_link response contained no cmd".to_string(),
        });
    };
    let url = crate::iptv::stalker::cmd_parser::extract_url_from_cmd(&raw_cmd)?;
    let url = repair_live_stream_target(url, source_cmd, kind);
    let scheme = validate_playable_scheme(&url)?;
    if !scheme_is_playable(scheme) {
        return Err(StalkerError::UnsupportedScheme { scheme: scheme.to_string() });
    }
    Ok(StalkerResolvedStream {
        stream_url: url,
        stream_kind: kind,
        playback_mode: requested_mode,
        candidates: vec![raw_cmd],
    })
}

fn repair_live_stream_target(url: String, source_cmd: &str, kind: StalkerStreamKind) -> String {
    if kind != StalkerStreamKind::Live {
        return url;
    }
    let Ok(resolved) = Url::parse(&url) else { return url };
    if !resolved.path().to_ascii_lowercase().ends_with("/play/live.php") {
        return url;
    }
    let needs_target = resolved
        .query_pairs()
        .find(|(key, _)| key.eq_ignore_ascii_case("stream"))
        .is_none_or(|(_, value)| !is_usable_stream_target(&value));
    if !needs_target {
        return url;
    }
    let Some(target) = source_live_stream_target(source_cmd) else { return url };
    upsert_raw_query_parameter(&url, "stream", &target)
}

fn source_live_stream_target(source_cmd: &str) -> Option<String> {
    let source_url = crate::iptv::stalker::cmd_parser::extract_url_from_cmd(source_cmd).ok()?;
    let parsed = Url::parse(&source_url).ok()?;
    if let Some(target) = parsed
        .query_pairs()
        .find_map(|(key, value)| key.eq_ignore_ascii_case("stream").then_some(value))
        .filter(|value| is_usable_stream_target(value))
    {
        return Some(target.into_owned());
    }
    let mut channel_segment = false;
    for segment in parsed.path_segments()? {
        if channel_segment {
            let target = segment.trim_end_matches('_');
            return is_usable_stream_target(target).then(|| target.to_string());
        }
        channel_segment = segment.eq_ignore_ascii_case("ch");
    }
    None
}

fn is_usable_stream_target(value: &str) -> bool {
    let value = value.trim();
    !value.is_empty() && value != "0" && !value.eq_ignore_ascii_case("null")
}

fn upsert_raw_query_parameter(url: &str, name: &str, value: &str) -> String {
    let (without_fragment, fragment) = url.split_once('#').map_or((url, None), |(base, fragment)| (base, Some(fragment)));
    let (base, query) = without_fragment.split_once('?').unwrap_or((without_fragment, ""));
    let mut result = String::with_capacity(url.len() + value.len());
    result.push_str(base);
    result.push('?');
    let mut replaced = false;
    for part in query.split('&').filter(|part| !part.is_empty()) {
        if result.as_bytes().last() != Some(&b'?') {
            result.push('&');
        }
        let key = part.split_once('=').map_or(part, |(key, _)| key);
        if key.eq_ignore_ascii_case(name) && !replaced {
            result.push_str(key);
            result.push('=');
            result.push_str(value);
            replaced = true;
        } else {
            result.push_str(part);
        }
    }
    if !replaced {
        if result.as_bytes().last() != Some(&b'?') {
            result.push('&');
        }
        result.push_str(name);
        result.push('=');
        result.push_str(value);
    }
    if let Some(fragment) = fragment {
        result.push('#');
        result.push_str(fragment);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::iptv::stalker::client::strip_jsonp;

    #[test]
    fn resolves_create_link_response_with_js_cmd() {
        use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
        let cmd = BASE64.encode(b"ffmpeg http://portal.example/stream/42");
        let body = serde_json::to_string(&serde_json::json!({
            "js": { "cmd": cmd }
        }))
        .unwrap();
        let parsed: StalkerCreateLinkResponse = serde_json::from_str(&body).unwrap();
        let resolved = resolve_response_test(parsed, StalkerStreamKind::Live).expect("ok");
        assert_eq!(resolved.stream_url, "http://portal.example/stream/42");
        assert_eq!(resolved.stream_kind, StalkerStreamKind::Live);
    }

    #[test]
    fn resolve_rejects_missing_cmd() {
        let body = serde_json::to_string(&serde_json::json!({"js": {}})).unwrap();
        let parsed: StalkerCreateLinkResponse = serde_json::from_str(&body).unwrap();
        let err = resolve_response_test(parsed, StalkerStreamKind::Live).expect_err("err");
        assert!(matches!(err, StalkerError::PortalRefusedCmd { .. }));
    }

    #[test]
    fn resolve_rejects_unsupported_scheme() {
        use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
        let cmd = BASE64.encode(b"ffmpeg file:///etc/passwd");
        let body = serde_json::to_string(&serde_json::json!({
            "js": { "cmd": cmd }
        }))
        .unwrap();
        let parsed: StalkerCreateLinkResponse = serde_json::from_str(&body).unwrap();
        let err = resolve_response_test(parsed, StalkerStreamKind::Movie).expect_err("err");
        assert!(matches!(err, StalkerError::UnsupportedScheme { .. }));
    }

    fn resolve_response_test(
        resp: StalkerCreateLinkResponse,
        kind: StalkerStreamKind,
    ) -> StalkerResult<StalkerResolvedStream> {
        resolve_response(resp, kind, StalkerPlaybackMode::DirectUrl, "")
    }

    #[test]
    fn strip_jsonp_does_not_corrupt_object_body() {
        let s = strip_jsonp(b"{\"js\":{\"cmd\":\"aGVsbG8=\"}}");
        assert_eq!(s, "{\"js\":{\"cmd\":\"aGVsbG8=\"}}");
    }

    #[test]
    fn resolve_preserves_requested_nginx_mode() {
        use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
        let cmd = BASE64.encode(b"ffmpeg http://portal.example/stream/42");
        let body = serde_json::to_string(&serde_json::json!({
            "js": { "cmd": cmd }
        }))
        .unwrap();
        let parsed: StalkerCreateLinkResponse = serde_json::from_str(&body).unwrap();
        let resolved = resolve_response(parsed, StalkerStreamKind::Live, StalkerPlaybackMode::TempLinkNginx, "")
            .expect("ok");
        assert_eq!(resolved.playback_mode, StalkerPlaybackMode::TempLinkNginx);
    }

    #[test]
    fn repair_blank_live_stream_from_source_channel_command() {
        let body = serde_json::json!({
            "js": {
                "cmd": "ffmpeg http://line.example/play/live.php?mac=00:11:22:33:44:55&stream=&extension=ts&play_token=abc&quality=hd"
            }
        });
        let parsed: StalkerCreateLinkResponse = serde_json::from_value(body).expect("response");

        let resolved = resolve_response(
            parsed,
            StalkerStreamKind::Live,
            StalkerPlaybackMode::DirectUrl,
            "ffmpeg http://localhost/ch/590_",
        )
            .expect("resolved response");

        assert_eq!(
            resolved.stream_url,
            "http://line.example/play/live.php?mac=00:11:22:33:44:55&stream=590&extension=ts&play_token=abc&quality=hd"
        );
    }

    #[test]
    fn usable_live_and_movie_stream_targets_are_unchanged() {
        for (kind, url) in [
            (StalkerStreamKind::Live, "http://line.example/play/live.php?stream=591&play_token=abc"),
            (StalkerStreamKind::Movie, "http://line.example/play/movie.php?stream=&play_token=abc"),
        ] {
            let parsed: StalkerCreateLinkResponse = serde_json::from_value(serde_json::json!({
                "js": {"cmd": format!("ffmpeg {url}")}
            }))
            .expect("response");

            let resolved = resolve_response(parsed, kind, StalkerPlaybackMode::DirectUrl, "")
                .expect("resolved response");

            assert_eq!(resolved.stream_url, url);
        }
    }
}
