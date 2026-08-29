//! Stable recording stream URLs.
//!
//! The recording engine resolves its own execution URL from the persisted
//! server-owned source identity. The URL is built here rather than in the
//! HTTP layer so `backend/dvr` does not have to reach back into the app.

use shared::model::XtreamCluster;
use tuliprox_auth::create_access_token;
use tuliprox_core::model::AppConfig;
use url::Url;

/// Access-token lifetime for a recording execution URL, in seconds.
const RECORDING_URL_TOKEN_TTL_SECS: u16 = 30;

fn build_recording_stream_url(
    base_url: &str,
    access_token: &str,
    target_name: &str,
    input_name: &str,
    virtual_id: u32,
    cluster: XtreamCluster,
) -> Option<String> {
    let mut url = Url::parse(base_url).ok()?;
    url.path_segments_mut().ok()?.pop_if_empty().extend([
        "api",
        "v1",
        "playlist",
        "recording",
        access_token,
        cluster.as_stream_type(),
        &virtual_id.to_string(),
    ]);
    url.query_pairs_mut().append_pair("target_name", target_name).append_pair("input_name", input_name);
    Some(url.into())
}

/// Build the stable recording URL for a server-resolved source. Stable means
/// it survives a playlist refresh: it carries the target and input names, not
/// a runtime id.
pub fn build_stable_recording_url(
    app_config: &AppConfig,
    target_name: &str,
    input_name: &str,
    virtual_id: u32,
    cluster: XtreamCluster,
) -> Option<String> {
    let access_token = create_access_token(&app_config.access_token_secret, RECORDING_URL_TOKEN_TTL_SECS);
    let config = app_config.config.load();
    let server_name = config
        .web_ui
        .as_ref()
        .and_then(|web_ui| web_ui.player_server.as_ref())
        .map_or("default", |server_name| server_name.as_str());
    let server_info = app_config.get_server_info(server_name)?;
    build_recording_stream_url(&server_info.get_base_url(), &access_token, target_name, input_name, virtual_id, cluster)
}

#[cfg(test)]
mod tests {
    use super::build_recording_stream_url;
    use shared::model::XtreamCluster;

    #[test]
    fn recording_stream_url_encodes_stable_names_without_runtime_id() {
        let url = build_recording_stream_url(
            "http://localhost:8901",
            "token-1",
            "My Target",
            "Input A",
            42,
            XtreamCluster::Video,
        )
        .expect("url");
        assert!(url.starts_with("http://localhost:8901/api/v1/playlist/recording/token-1/movie/42?"), "{url}");
        assert!(url.contains("target_name=My+Target"), "{url}");
        assert!(url.contains("input_name=Input+A"), "{url}");
    }

    #[test]
    fn recording_stream_url_rejects_an_unparsable_base() {
        assert!(build_recording_stream_url("not a url", "t", "target", "input", 1, XtreamCluster::Live).is_none());
    }
}
