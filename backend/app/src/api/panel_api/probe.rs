use super::{find_input_by_provider_name, try_provision_account_on_exhausted};
use crate::{
    api::model::AppState,
    model::{ConfigInput, InputSource},
    utils::debug_if_enabled,
};
use axum::http::Method;
use shared::{
    concat_string,
    error::TuliproxError,
    model::{DisconnectReason, PanelApiProvisioningMethod, VirtualId},
    utils::sanitize_sensitive_info,
};
use std::{net::SocketAddr, sync::Arc, time::Duration};
use tokio_util::sync::CancellationToken;
use url::Url;

pub(super) fn provisioning_method_to_reqwest(method: PanelApiProvisioningMethod) -> Method {
    match method {
        PanelApiProvisioningMethod::Head => Method::HEAD,
        PanelApiProvisioningMethod::Get => Method::GET,
        PanelApiProvisioningMethod::Post => Method::POST,
    }
}

pub(super) fn build_player_api_action_url(base_url: &str, username: &str, password: &str, action: &str) -> Option<Url> {
    let url = Url::parse(base_url).ok()?;
    let host = url.host_str()?;
    let scheme = url.scheme();
    let mut base = concat_string!(scheme, "://", host);
    if let Some(port) = url.port() {
        base.push(':');
        base.push_str(&port.to_string());
    }
    base.push_str("/player_api.php");
    let mut test_url = Url::parse(&base).ok()?;
    test_url
        .query_pairs_mut()
        .append_pair("username", username)
        .append_pair("password", password)
        .append_pair("action", action);
    Some(test_url)
}

pub(super) enum PanelApiProbeTarget {
    PlayerApi { action: &'static str, input_source: InputSource },
}

impl PanelApiProbeTarget {
    pub(super) fn action(&self) -> &'static str {
        match self {
            PanelApiProbeTarget::PlayerApi { action, .. } => action,
        }
    }
}

pub(super) fn format_probe_target_actions(targets: &[PanelApiProbeTarget]) -> String {
    let mut result = String::new();
    for (idx, target) in targets.iter().enumerate() {
        if idx > 0 {
            result.push(',');
        }
        result.push_str(target.action());
    }
    result
}

pub(super) fn build_panel_api_probe_targets(
    input: &ConfigInput,
    username: &str,
    password: &str,
) -> Vec<PanelApiProbeTarget> {
    let mut targets = Vec::new();
    for action in ["client_info", "get_live_categories", "get_series_categories", "get_vod_categories"] {
        if let Some(url) = build_player_api_action_url(input.url.as_str(), username, password, action) {
            targets.push(PanelApiProbeTarget::PlayerApi {
                action,
                input_source: InputSource::from(input).with_url(url.to_string()),
            });
        }
    }
    targets
}

#[allow(clippy::too_many_lines)]
pub(crate) async fn run_panel_api_provisioning_probe(
    app_state: Arc<AppState>,
    input_name: Arc<str>,
    stop_signal: CancellationToken,
    addr: SocketAddr,
    virtual_id: VirtualId,
) -> Result<(), TuliproxError> {
    if stop_signal.is_cancelled() {
        return Ok(());
    }
    let provisioning_kick_secs = 0;
    let Some(input) = find_input_by_provider_name(app_state.as_ref(), input_name.as_ref()) else {
        debug_if_enabled!(
            "panel_api provisioning probe skipped (input no longer exists) for input {}",
            sanitize_sensitive_info(input_name.as_ref())
        );
        stop_signal.cancel();
        let _ = app_state
            .connection_manager
            .close_connection_with_reason_and_block(
                &addr,
                virtual_id,
                provisioning_kick_secs,
                DisconnectReason::Provisioning,
            )
            .await;
        return Ok(());
    };

    let Some(panel_cfg) = input.panel_api.as_ref() else {
        debug_if_enabled!(
            "panel_api provisioning probe skipped (missing config) for input {}",
            sanitize_sensitive_info(&input.name)
        );
        stop_signal.cancel();
        let _ = app_state
            .connection_manager
            .close_connection_with_reason_and_block(
                &addr,
                virtual_id,
                provisioning_kick_secs,
                DisconnectReason::Provisioning,
            )
            .await;
        return Ok(());
    };
    if !panel_cfg.enabled {
        debug_if_enabled!(
            "panel_api provisioning probe skipped (panel_api.enabled false) for input {}",
            sanitize_sensitive_info(&input.name)
        );
        stop_signal.cancel();
        let _ = app_state
            .connection_manager
            .close_connection_with_reason_and_block(
                &addr,
                virtual_id,
                provisioning_kick_secs,
                DisconnectReason::Provisioning,
            )
            .await;
        return Ok(());
    }
    if panel_cfg.url.trim().is_empty() {
        debug_if_enabled!(
            "panel_api provisioning probe skipped (panel_api.url empty) for input {}",
            sanitize_sensitive_info(&input.name)
        );
        stop_signal.cancel();
        let _ = app_state
            .connection_manager
            .close_connection_with_reason_and_block(
                &addr,
                virtual_id,
                provisioning_kick_secs,
                DisconnectReason::Provisioning,
            )
            .await;
        return Ok(());
    }

    let max_wait_secs = panel_cfg.provisioning.timeout_sec;

    debug_if_enabled!(
        "panel_api provisioning probe start for input {} (timeout={}s)",
        sanitize_sensitive_info(&input.name),
        max_wait_secs
    );

    let outcome = tokio::select! {
        () = stop_signal.cancelled() => return Ok(()),
        outcome = try_provision_account_on_exhausted(&app_state, &input_name) => outcome,
    };

    if let Some(outcome) = outcome.as_ref() {
        debug_if_enabled!(
            "panel_api provisioning {} completed for input {}",
            outcome.kind_label(),
            sanitize_sensitive_info(&input.name)
        );
    } else {
        debug_if_enabled!(
            "panel_api provisioning failed for input {}; waiting for timeout",
            sanitize_sensitive_info(&input.name)
        );
    }

    if outcome.is_none() {
        if max_wait_secs > 0 {
            tokio::select! {
                () = stop_signal.cancelled() => return Ok(()),
                () = tokio::time::sleep(Duration::from_secs(max_wait_secs)) => {}
            }
        }
        debug_if_enabled!(
            "panel_api provisioning probe timeout reached for input {} (no credentials)",
            sanitize_sensitive_info(&input.name)
        );
        stop_signal.cancel();
        debug_if_enabled!(
            "panel_api provisioning closing client connection for input {} addr={}",
            sanitize_sensitive_info(&input.name),
            sanitize_sensitive_info(&addr.to_string())
        );
        let _ = app_state
            .connection_manager
            .close_connection_with_reason_and_block(
                &addr,
                virtual_id,
                provisioning_kick_secs,
                DisconnectReason::Provisioning,
            )
            .await;
        return Ok(());
    }

    if stop_signal.is_cancelled() {
        return Ok(());
    }
    stop_signal.cancel();
    debug_if_enabled!(
        "panel_api provisioning closing client connection for input {} addr={}",
        sanitize_sensitive_info(&input.name),
        sanitize_sensitive_info(&addr.to_string())
    );
    let _ = app_state
        .connection_manager
        .close_connection_with_reason_and_block(
            &addr,
            virtual_id,
            provisioning_kick_secs,
            DisconnectReason::Provisioning,
        )
        .await;
    Ok(())
}
