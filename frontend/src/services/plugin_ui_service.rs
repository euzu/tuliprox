//! REST client for the plugin UI: contributions, locales, rpc, metrics. W6b lane U.
use crate::{
    error::{Error, ErrorInfo},
    services::{get_base_href, get_token, request_get},
};
use gloo_net::http::Request;
use serde_json::Value;
use shared::{
    model::{PluginMetricDto, PluginRpcErrorDto, PluginSlotDto, PluginUiDto},
    utils::{concat_path_leading_slash, CONTENT_TYPE_JSON},
};
use std::collections::BTreeMap;

/// A plugin locale file: a flat map of key to text.
pub type PluginLocaleMap = BTreeMap<String, String>;

/// Why an rpc call did not produce a result. `status` is the HTTP status, or 0
/// when the request never got an answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginRpcFailure {
    pub status: u16,
    pub error: String,
}

/// The `slot` query value of a frame, the serialized form of the slot.
pub const fn plugin_slot_id(slot: PluginSlotDto) -> &'static str {
    match slot {
        PluginSlotDto::Menu => "menu",
        PluginSlotDto::ConfigSection => "config-section",
        PluginSlotDto::ConfigTab => "config-tab",
        PluginSlotDto::SourceEditorTile => "source-editor-tile",
        PluginSlotDto::DashboardCard => "dashboard-card",
        PluginSlotDto::PlaylistExplorerAction => "playlist-explorer-action",
        PluginSlotDto::ChannelDetailPanel => "channel-detail-panel",
        PluginSlotDto::EpgOverlay => "epg-overlay",
        PluginSlotDto::SetupWizardStep => "setup-wizard-step",
    }
}

/// Percent-encodes everything but the RFC 3986 unreserved characters.
fn percent_encode(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(char::from(byte));
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

/// The frame document of a contribution relative to `<plugin>/ui/`: the
/// manifest `element` without its `ui/` prefix. `None` for anything that could
/// leave that directory or is not a plain relative path.
pub fn frame_document(element: &str) -> Option<&str> {
    let path = element.strip_prefix("ui/").unwrap_or(element);
    let plain = !path.is_empty()
        && !path.contains(['\\', '\0', '?', '#'])
        && path.split('/').all(|segment| !segment.is_empty() && segment != "." && segment != "..");
    plain.then_some(path)
}

/// `{base}plugins/{id}/ui/{element minus "ui/"}?v={version}&slot={slot}`. The
/// base is the page's base href, which always ends in `/`.
pub fn frame_src(
    base_href: &str,
    plugin_id: &str,
    version: &str,
    element: &str,
    slot: PluginSlotDto,
) -> Option<String> {
    let document = frame_document(element)?;
    let path = document.split('/').map(percent_encode).collect::<Vec<_>>().join("/");
    Some(format!(
        "{base_href}plugins/{}/ui/{path}?v={}&slot={}",
        percent_encode(plugin_id),
        percent_encode(version),
        plugin_slot_id(slot)
    ))
}

/// The error text of a failed rpc: the `error` of the body when it has one.
fn rpc_error_text(status: u16, body: &str) -> String {
    let message = serde_json::from_str::<PluginRpcErrorDto>(body)
        .map(|dto| dto.error)
        .or_else(|_| serde_json::from_str::<ErrorInfo>(body).map(|info| info.error))
        .unwrap_or_else(|_| body.trim().to_owned());
    if message.trim().is_empty() {
        format!("HTTP {status}")
    } else {
        message
    }
}

pub struct PluginUiService {
    plugins_path: String,
    base_href: String,
}

impl PluginUiService {
    pub fn new() -> Self { Self::with_base(get_base_href()) }

    /// A client for an app served below `base_href` (which ends in `/`).
    pub fn with_base(base_href: String) -> Self {
        Self { plugins_path: concat_path_leading_slash(&base_href, "api/v1/plugins"), base_href }
    }

    /// `GET /api/v1/plugins/ui`: what the web UI renders for every plugin with a `ui:` part.
    pub async fn list(&self) -> Result<Vec<PluginUiDto>, Error> {
        request_get(&format!("{}/ui", self.plugins_path), None, None).await.map(Option::unwrap_or_default)
    }

    /// `GET /api/v1/plugins/metrics`. The route belongs to the metrics wave and
    /// may not exist: every failure is an empty list.
    pub async fn metrics(&self) -> Vec<PluginMetricDto> {
        request_get(&format!("{}/metrics", self.plugins_path), None, None).await.ok().flatten().unwrap_or_default()
    }

    /// One `ui/locales/<lang>.json` of a plugin.
    pub async fn locale(&self, plugin_id: &str, version: &str, lang: &str) -> Result<PluginLocaleMap, Error> {
        let path = format!(
            "plugins/{}/ui/locales/{}.json?v={}",
            percent_encode(plugin_id),
            percent_encode(lang),
            percent_encode(version)
        );
        request_get(&format!("{}{path}", self.base_href), None, None).await.map(Option::unwrap_or_default)
    }

    pub fn frame_src(&self, plugin_id: &str, version: &str, element: &str, slot: PluginSlotDto) -> Option<String> {
        frame_src(&self.base_href, plugin_id, version, element, slot)
    }

    /// `POST /api/v1/plugins/{id}/rpc/{method}`, the way every other request of
    /// the app is made: same base URL, same bearer token. It is not
    /// `requests.rs::request_post` because the frame is told the HTTP status and
    /// that helper folds most of them into one error.
    pub async fn rpc(&self, plugin_id: &str, method: &str, payload: &Value) -> Result<Value, PluginRpcFailure> {
        let failure = |status: u16, error: &str| PluginRpcFailure { status, error: error.to_owned() };
        let url = format!("{}/{}/rpc/{}", self.plugins_path, percent_encode(plugin_id), percent_encode(method));
        let body = serde_json::to_string(payload).map_err(|_| failure(400, "payload is not JSON"))?;
        let mut request = Request::post(&url).header("Accept", CONTENT_TYPE_JSON);
        if let Some(token) = get_token() {
            request = request.header("Authorization", format!("Bearer {token}").as_str());
        }
        let request = request
            .header("Content-Type", CONTENT_TYPE_JSON)
            .body(body)
            .map_err(|_| failure(0, "request could not be built"))?;
        let response = request.send().await.map_err(|_| failure(0, "request failed"))?;
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        if (200..300).contains(&status) {
            if text.trim().is_empty() {
                return Ok(Value::Null);
            }
            return serde_json::from_str(&text).map_err(|_| failure(502, "the result is not JSON"));
        }
        Err(PluginRpcFailure { status, error: rpc_error_text(status, &text) })
    }
}

impl Default for PluginUiService {
    fn default() -> Self { Self::new() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_src_strips_the_ui_prefix_and_carries_version_and_slot() {
        assert_eq!(
            frame_src("/", "sports", "1.2.3", "ui/sports-guide.html", PluginSlotDto::EpgOverlay).as_deref(),
            Some("/plugins/sports/ui/sports-guide.html?v=1.2.3&slot=epg-overlay")
        );
        assert_eq!(
            frame_src("/tuli/", "p", "1", "pages/a b.html", PluginSlotDto::Menu).as_deref(),
            Some("/tuli/plugins/p/ui/pages/a%20b.html?v=1&slot=menu")
        );
    }

    #[test]
    fn frame_src_refuses_anything_that_leaves_the_ui_directory() {
        for element in ["", "ui/", "ui/../x.html", "../x.html", "a//b.html", "./a.html", "a\\b.html", "a.html?x", "a#b"]
        {
            assert_eq!(frame_src("/", "p", "1", element, PluginSlotDto::Menu), None, "{element}");
        }
    }

    #[test]
    fn percent_encoding_keeps_only_unreserved_characters() {
        assert_eq!(percent_encode("a-b_c.d~e"), "a-b_c.d~e");
        assert_eq!(percent_encode("a/b?c&d"), "a%2Fb%3Fc%26d");
        assert_eq!(percent_encode("\u{e9}"), "%C3%A9");
    }

    #[test]
    fn slot_ids_are_the_serialized_slots() {
        for slot in [
            PluginSlotDto::Menu,
            PluginSlotDto::ConfigSection,
            PluginSlotDto::ConfigTab,
            PluginSlotDto::SourceEditorTile,
            PluginSlotDto::DashboardCard,
            PluginSlotDto::PlaylistExplorerAction,
            PluginSlotDto::ChannelDetailPanel,
            PluginSlotDto::EpgOverlay,
            PluginSlotDto::SetupWizardStep,
        ] {
            assert_eq!(serde_json::to_value(slot).unwrap(), plugin_slot_id(slot));
        }
    }

    #[test]
    fn rpc_error_text_prefers_the_error_field() {
        assert_eq!(rpc_error_text(422, r#"{"error":"bad input","fields":[]}"#), "bad input");
        assert_eq!(rpc_error_text(502, "upstream gone"), "upstream gone");
        assert_eq!(rpc_error_text(503, "  "), "HTTP 503");
    }
}
