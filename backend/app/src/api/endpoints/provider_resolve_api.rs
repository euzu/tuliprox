use crate::{
    api::{api_utils::redirect, model::AppState},
    auth::{check_network_access_only, Fingerprint},
    model::{
        resolve_provider_scheme_url_with_provider_index, ApiProxyServerInfo, ConfigInput, ConfigTarget,
        ProxyUserCredentials,
    },
    repository::{m3u_get_item_for_stream_id, storage_const, xtream_get_item_for_stream_id},
    utils::{
        decode_provider_resolve_token, extract_extension_from_url, sanitize_sensitive_info, ProviderResolveToken,
        PROVIDER_RESOLVE_ROUTE_PREFIX,
    },
};
use axum::{extract, response::IntoResponse};
use log::{debug, error};
use shared::{
    error::TuliproxError,
    model::{PlaylistItemType, TargetType, XtreamCluster},
};
use std::{borrow::Cow, sync::Arc};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProviderResolveOutputKind {
    Xtream,
    M3u,
}

#[derive(Clone, Copy)]
struct ProviderResolveItem<'a> {
    virtual_id: u32,
    item_type: PlaylistItemType,
    cluster: XtreamCluster,
    url: &'a str,
}

fn stream_type_for_item(item: ProviderResolveItem<'_>) -> &'static str {
    match item.item_type {
        PlaylistItemType::Live | PlaylistItemType::LiveHls | PlaylistItemType::LiveDash => "live",
        PlaylistItemType::Video | PlaylistItemType::LocalVideo => "movie",
        PlaylistItemType::Series
        | PlaylistItemType::SeriesInfo
        | PlaylistItemType::LocalSeries
        | PlaylistItemType::LocalSeriesInfo => "series",
        _ => match item.cluster {
            XtreamCluster::Live => "live",
            XtreamCluster::Video => "movie",
            XtreamCluster::Series => "series",
        },
    }
}

fn provider_resolve_output_kind(target: &ConfigTarget) -> Option<ProviderResolveOutputKind> {
    if target.has_output(TargetType::Xtream) {
        Some(ProviderResolveOutputKind::Xtream)
    } else if target.has_output(TargetType::M3u) {
        Some(ProviderResolveOutputKind::M3u)
    } else {
        None
    }
}

fn resolve_direct_provider_location(input: &ConfigInput, url: &str) -> Result<String, TuliproxError> {
    if let Some(provider) = input.get_resolve_provider(url) {
        let (_, resolved) = resolve_provider_scheme_url_with_provider_index(
            url,
            Some(Arc::clone(&provider)),
            provider.get_current_index(),
        )?;
        return Ok(resolved.into_owned());
    }
    input.resolve_url(url).map(Cow::into_owned)
}

fn resolve_provider_playlist_item_location(
    user: &ProxyUserCredentials,
    force_redirect: bool,
    output_kind: ProviderResolveOutputKind,
    server_info: &ApiProxyServerInfo,
    input: &ConfigInput,
    item: ProviderResolveItem<'_>,
) -> Result<String, TuliproxError> {
    let redirect = user.proxy.is_redirect(item.item_type) || force_redirect;
    if redirect {
        return resolve_direct_provider_location(input, item.url);
    }

    let stream_type = stream_type_for_item(item);
    let ext = extract_extension_from_url(item.url).unwrap_or_default();
    let base_url = server_info.get_base_url();
    Ok(match output_kind {
        ProviderResolveOutputKind::Xtream => {
            format!("{base_url}/{stream_type}/{}/{}/{}{}", user.username, user.password, item.virtual_id, ext)
        }
        ProviderResolveOutputKind::M3u => format!(
            "{base_url}/{}/{stream_type}/{}/{}/{}{}",
            storage_const::M3U_STREAM_PATH,
            user.username,
            user.password,
            item.virtual_id,
            ext
        ),
    })
}

struct ProviderResolveLoadedItem {
    virtual_id: u32,
    item_type: PlaylistItemType,
    cluster: XtreamCluster,
    url: Arc<str>,
    input_name: Arc<str>,
    /// Whether the user's compiled content filter permits this item.
    content_allowed: bool,
}

async fn load_provider_resolve_item(
    output_kind: ProviderResolveOutputKind,
    decoded_virtual_id: u32,
    decoded_cluster: XtreamCluster,
    app_state: &Arc<AppState>,
    target: &ConfigTarget,
    user: &ProxyUserCredentials,
) -> Result<ProviderResolveLoadedItem, TuliproxError> {
    match output_kind {
        ProviderResolveOutputKind::Xtream => xtream_get_item_for_stream_id(
            decoded_virtual_id,
            &app_state.app_config,
            &app_state.playlists,
            target,
            Some(decoded_cluster),
        )
        .await
        .map(|item| ProviderResolveLoadedItem {
            virtual_id: item.virtual_id.get(),
            item_type: item.item_type,
            cluster: item.xtream_cluster,
            content_allowed: user.t_filter.is_none() || user.allows_content(&shared::model::PlaylistItem::from(&item)),
            url: item.url,
            input_name: item.input_name,
        })
        .map_err(|err| TuliproxError::RepositoryXtream(err.to_string())),
        ProviderResolveOutputKind::M3u => {
            let item =
                m3u_get_item_for_stream_id(decoded_virtual_id, &app_state.app_config, &app_state.playlists, target)
                    .await
                    .map_err(|err| TuliproxError::RepositoryM3u(err.to_string()))?;
            if !item.item_type.is_cluster(decoded_cluster) {
                return Err(TuliproxError::RepositoryM3u(format!(
                    "M3U item {} is not in requested cluster {}",
                    item.virtual_id, decoded_cluster
                )));
            }
            Ok(ProviderResolveLoadedItem {
                virtual_id: item.virtual_id.get(),
                item_type: item.item_type,
                cluster: decoded_cluster,
                content_allowed: user.t_filter.is_none()
                    || user.allows_content(&shared::model::PlaylistItem::from(&item)),
                url: item.url,
                input_name: item.input_name,
            })
        }
    }
}

async fn provider_resolve(
    fingerprint: Fingerprint,
    extract::Path(token): extract::Path<String>,
    extract::State(app_state): extract::State<Arc<AppState>>,
) -> impl IntoResponse + Send {
    let secret = app_state.get_encrypt_secret();
    let decoded = match decode_provider_resolve_token(&secret, &token) {
        Ok(decoded) => decoded,
        Err(err) => {
            debug!("Invalid provider resolve token: {err}");
            return axum::http::StatusCode::BAD_REQUEST.into_response();
        }
    };

    let ProviderResolveToken::PlaylistItem(decoded) = decoded;
    let Some((user, target)) = app_state.app_config.get_target_for_username(&decoded.username) else {
        return axum::http::StatusCode::FORBIDDEN.into_response();
    };
    if target.id != decoded.target_id {
        return axum::http::StatusCode::FORBIDDEN.into_response();
    }
    if let Err(err) = check_network_access_only(&user, &fingerprint, &app_state.app_config, &app_state.geoip) {
        return err.into_player_response(app_state.app_config.get_auth_error_status());
    }
    if user.permission_denied(&app_state.app_config) || !user.allows_cluster(decoded.cluster) {
        return axum::http::StatusCode::FORBIDDEN.into_response();
    }
    let Some(output_kind) = provider_resolve_output_kind(&target) else {
        debug!("Provider resolve target '{}' has no xtream or m3u output", target.name);
        return axum::http::StatusCode::BAD_REQUEST.into_response();
    };
    let Some(server_info) = app_state.app_config.get_user_server_info(user.as_ref()) else {
        error!("Provider resolve user '{}' has no server info", sanitize_sensitive_info(&user.username));
        return axum::http::StatusCode::BAD_REQUEST.into_response();
    };

    let item =
        match load_provider_resolve_item(output_kind, decoded.virtual_id, decoded.cluster, &app_state, &target, &user)
            .await
        {
            Ok(item) => item,
            Err(err) => {
                debug!("Provider resolve item lookup failed: {err}");
                return axum::http::StatusCode::NOT_FOUND.into_response();
            }
        };
    if !user.allows_item_type(item.item_type) || !item.content_allowed {
        return axum::http::StatusCode::FORBIDDEN.into_response();
    }
    let Some(input) = app_state.app_config.get_input_by_name(&item.input_name) else {
        error!("Provider resolve input '{}' is missing", sanitize_sensitive_info(&item.input_name));
        return axum::http::StatusCode::BAD_REQUEST.into_response();
    };

    match resolve_provider_playlist_item_location(
        user.as_ref(),
        target.is_force_redirect(item.item_type),
        output_kind,
        &server_info,
        &input,
        ProviderResolveItem {
            virtual_id: item.virtual_id,
            item_type: item.item_type,
            cluster: item.cluster,
            url: &item.url,
        },
    ) {
        Ok(location) => redirect(&location).into_response(),
        Err(err) => {
            error!("Provider resolve failed: {}", sanitize_sensitive_info(&err.to_string()));
            axum::http::StatusCode::BAD_REQUEST.into_response()
        }
    }
}

pub fn provider_resolve_api_register() -> axum::Router<Arc<AppState>> {
    axum::Router::new()
        .route(&format!("{PROVIDER_RESOLVE_ROUTE_PREFIX}/{{token}}"), axum::routing::get(provider_resolve))
}

#[cfg(test)]
mod tests {
    use super::{resolve_provider_playlist_item_location, ProviderResolveItem, ProviderResolveOutputKind};
    use crate::model::{ApiProxyServerInfo, ConfigInput, ConfigProvider, ProxyUserCredentials};
    use shared::model::{
        ConfigProviderDto, InputType, PlaylistItemType, ProviderUrlSelectionPolicy, ProxyType, XtreamCluster,
    };
    use std::{collections::HashMap, sync::Arc};

    fn server_info() -> ApiProxyServerInfo {
        ApiProxyServerInfo {
            name: "default".to_string(),
            protocol: "http".to_string(),
            host: "proxy.example.com".to_string(),
            port: None,
            timezone: "UTC".to_string(),
            message: String::new(),
            path: None,
        }
    }

    fn user(proxy: ProxyType) -> ProxyUserCredentials {
        let mut user = ProxyUserCredentials::default();
        user.username = "alice".to_string();
        user.password = "secret".to_string();
        user.proxy = proxy;
        user
    }

    fn input() -> ConfigInput { input_with_provider_urls(vec!["http://provider.example.com".into()], None) }

    fn input_with_provider_urls(urls: Vec<Arc<str>>, current_index: Option<usize>) -> ConfigInput {
        let provider = Arc::new(ConfigProvider::from(&ConfigProviderDto {
            name: "myprovider".into(),
            urls,
            provider_url_selection_policy: ProviderUrlSelectionPolicy::ResumeLastWorking,
            dns: None,
        }));
        if let Some(index) = current_index {
            provider.set_current_index(index);
        }

        ConfigInput {
            name: Arc::from("input-a"),
            input_type: InputType::M3u,
            headers: HashMap::new(),
            url: "http://input.example.com".to_string(),
            provider_configs: Some(vec![provider]),
            ..Default::default()
        }
    }

    #[test]
    fn provider_resolve_redirect_user_returns_resolved_provider_location() {
        let item = ProviderResolveItem {
            virtual_id: 81_356,
            item_type: PlaylistItemType::Live,
            cluster: XtreamCluster::Live,
            url: "provider://myprovider/live/root/pass/81356.ts",
        };

        let location = resolve_provider_playlist_item_location(
            &user(ProxyType::Redirect),
            false,
            ProviderResolveOutputKind::Xtream,
            &server_info(),
            &input(),
            item,
        )
        .unwrap();

        assert_eq!(location, "http://provider.example.com/live/root/pass/81356.ts");
    }

    #[test]
    fn provider_resolve_redirect_user_uses_current_provider_url() {
        let item = ProviderResolveItem {
            virtual_id: 81_356,
            item_type: PlaylistItemType::Live,
            cluster: XtreamCluster::Live,
            url: "provider://myprovider/live/root/pass/81356.ts",
        };
        let input = input_with_provider_urls(
            vec!["http://provider-a.example.com".into(), "http://provider-b.example.com".into()],
            Some(1),
        );

        let location = resolve_provider_playlist_item_location(
            &user(ProxyType::Redirect),
            false,
            ProviderResolveOutputKind::Xtream,
            &server_info(),
            &input,
            item,
        )
        .unwrap();

        assert_eq!(location, "http://provider-b.example.com/live/root/pass/81356.ts");
    }

    #[test]
    fn provider_resolve_reverse_user_prefers_xtream_streaming_location() {
        let item = ProviderResolveItem {
            virtual_id: 81_356,
            item_type: PlaylistItemType::Video,
            cluster: XtreamCluster::Video,
            url: "provider://myprovider/movie/root/pass/81356.mkv",
        };

        let location = resolve_provider_playlist_item_location(
            &user(ProxyType::Reverse(None)),
            false,
            ProviderResolveOutputKind::Xtream,
            &server_info(),
            &input(),
            item,
        )
        .unwrap();

        assert_eq!(location, "http://proxy.example.com/movie/alice/secret/81356.mkv");
    }

    #[test]
    fn provider_resolve_reverse_user_uses_m3u_streaming_location_when_no_xtream_output_exists() {
        let item = ProviderResolveItem {
            virtual_id: 81_356,
            item_type: PlaylistItemType::Video,
            cluster: XtreamCluster::Video,
            url: "provider://myprovider/movie/root/pass/81356.mkv",
        };

        let location = resolve_provider_playlist_item_location(
            &user(ProxyType::Reverse(None)),
            false,
            ProviderResolveOutputKind::M3u,
            &server_info(),
            &input(),
            item,
        )
        .unwrap();

        assert_eq!(location, "http://proxy.example.com/m3u-stream/movie/alice/secret/81356.mkv");
    }
}
