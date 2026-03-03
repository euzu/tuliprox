use crate::{
    api::{
        api_utils::{internal_server_error, try_unwrap_body},
        config_file::ConfigFile,
        model::AppState,
    },
    model::{validate_library_paths_from_dto, ApiProxyConfig, InputSource},
    utils,
    utils::{
        persist_messaging_templates, prepare_sources_batch, prepare_users, read_api_proxy_file,
        request::download_text_content,
    },
};
use axum::{response::IntoResponse, Router};
use log::{error, warn};
use serde_json::json;
use shared::{
    error::TuliproxError,
    model::{ApiProxyConfigDto, ConfigDto, ConfigProviderDto, ProviderDnsDto, SourcesConfigDto},
};
use std::path::Path;
use std::sync::Arc;

fn inject_provider_dns_resolved(sources_dto: &mut SourcesConfigDto, runtime_sources: &crate::model::SourcesConfig) {
    let Some(provider_dtos) = sources_dto.provider.as_mut() else {
        return;
    };
    for provider_dto in provider_dtos {
        let Some(runtime_provider) = runtime_sources.get_provider_by_name(provider_dto.name.as_ref()) else {
            continue;
        };
        let Some(dns_dto) = provider_dto.dns.as_mut() else {
            continue;
        };
        if runtime_provider.get_dns_config().is_none() {
            dns_dto.resolved = None;
            continue;
        }
        let snapshot = runtime_provider.snapshot_resolved_ordered();
        dns_dto.resolved = (!snapshot.is_empty()).then_some(snapshot);
    }
}

fn strip_provider_dns_resolved(sources_dto: &mut SourcesConfigDto) {
    let Some(provider_dtos) = sources_dto.provider.as_mut() else {
        return;
    };
    for provider_dto in provider_dtos {
        if let Some(dns_dto) = provider_dto.dns.as_mut() {
            dns_dto.resolved = None;
        }
    }
}

fn merge_provider_dns_resolved_from_existing(
    sources_dto: &mut SourcesConfigDto,
    existing_sources_dto: &SourcesConfigDto,
) {
    let Some(provider_dtos) = sources_dto.provider.as_mut() else {
        return;
    };
    let Some(existing_provider_dtos) = existing_sources_dto.provider.as_ref() else {
        return;
    };

    let mut resolved_by_provider_name = std::collections::HashMap::new();
    for provider in existing_provider_dtos {
        let Some(dns) = provider.dns.as_ref() else {
            continue;
        };
        let Some(resolved) = dns.resolved.as_ref() else {
            continue;
        };
        if !resolved.is_empty() {
            resolved_by_provider_name.insert(provider.name.to_string(), resolved.clone());
        }
    }

    if resolved_by_provider_name.is_empty() {
        return;
    }

    for provider in provider_dtos {
        let Some(dns) = provider.dns.as_mut() else {
            continue;
        };
        if dns.resolved.is_some() {
            continue;
        }
        if let Some(resolved) = resolved_by_provider_name.get(provider.name.as_ref()) {
            dns.resolved = Some(resolved.clone());
        }
    }
}

fn build_existing_sources_from_runtime(runtime_sources: &crate::model::SourcesConfig) -> Option<SourcesConfigDto> {
    let providers: Vec<ConfigProviderDto> = runtime_sources
        .provider
        .iter()
        .filter_map(|runtime_provider| {
            let resolved = runtime_provider.snapshot_resolved_ordered();
            if resolved.is_empty() {
                return None;
            }

            Some(ConfigProviderDto {
                name: runtime_provider.name.clone(),
                urls: runtime_provider.urls.clone(),
                dns: Some(ProviderDnsDto {
                    enabled: runtime_provider.get_dns_config().is_some_and(|cfg| cfg.enabled),
                    resolved: Some(resolved),
                    ..ProviderDnsDto::default()
                }),
            })
        })
        .collect();

    if providers.is_empty() {
        None
    } else {
        Some(SourcesConfigDto {
            provider: Some(providers),
            ..SourcesConfigDto::default()
        })
    }
}

pub(in crate::api::endpoints) async fn intern_save_config_api_proxy(
    backup_dir: &str,
    api_proxy: &ApiProxyConfigDto,
    file_path: &str,
) -> Option<TuliproxError> {
    match utils::save_api_proxy(file_path, backup_dir, api_proxy).await {
        Ok(()) => {}
        Err(err) => {
            error!("Failed to save api_proxy.yml {err}");
            return Some(err);
        }
    }
    None
}

async fn intern_save_config_main(file_path: &str, backup_dir: &str, cfg: &ConfigDto) -> Option<TuliproxError> {
    match utils::save_main_config(file_path, backup_dir, cfg).await {
        Ok(()) => {}
        Err(err) => {
            error!("Failed to save config.yml {err}");
            return Some(err);
        }
    }
    None
}

async fn save_config_main(
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
    axum::extract::Json(mut cfg): axum::extract::Json<ConfigDto>,
) -> impl axum::response::IntoResponse + Send {
    if let Err(err) = cfg.prepare(false) {
        return (axum::http::StatusCode::BAD_REQUEST, axum::Json(json!({"error": err.to_string()}))).into_response();
    }
    if !cfg.is_valid() {
        (axum::http::StatusCode::BAD_REQUEST, axum::Json(json!({"error": "Invalid content"}))).into_response()
    } else if let Err(err) = validate_library_paths_from_dto(&cfg) {
        (axum::http::StatusCode::BAD_REQUEST, axum::Json(json!({"error": err.to_string()}))).into_response()
    } else {
        if let Err(err) = persist_messaging_templates(&app_state, &mut cfg).await {
            error!("Failed to persist messaging templates: {err}");
            return (axum::http::StatusCode::INTERNAL_SERVER_ERROR, axum::Json(json!({"error": err.to_string()})))
                .into_response();
        }

        let paths = app_state.app_config.paths.load();
        let file_path = paths.config_file_path.as_str();
        let config = app_state.app_config.config.load();
        let backup_dir = config.get_backup_dir();
        if let Some(err) = intern_save_config_main(file_path, backup_dir.as_ref(), &cfg).await {
            return (axum::http::StatusCode::INTERNAL_SERVER_ERROR, axum::Json(json!({"error": err.to_string()})))
                .into_response();
        }
        axum::http::StatusCode::OK.into_response()
    }
}

async fn save_config_sources(
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
    axum::extract::Json(mut sources): axum::extract::Json<SourcesConfigDto>,
) -> impl axum::response::IntoResponse + Send {
    let sources_file_path = {
        let paths = app_state.app_config.paths.load();
        paths.sources_file_path.clone()
    };
    let existing_sources_dto =
        match utils::read_sources_file_from_path(Path::new(&sources_file_path), false, false, None).await {
        Ok(existing) => Some(existing),
        Err(err) => {
            warn!("Failed to preload existing source.yml from '{sources_file_path}' before save: {err}");
            let runtime_sources = app_state.app_config.sources.load();
            let runtime_fallback = build_existing_sources_from_runtime(runtime_sources.as_ref());
            if runtime_fallback.is_some() {
                warn!("Using runtime provider dns snapshot as fallback for preserving dns.resolved");
            }
            runtime_fallback
        }
    };

    // `dns.resolved` is runtime-managed and must not be accepted from API input,
    // but existing runtime values should survive UI saves until the next DNS refresh updates them.
    strip_provider_dns_resolved(&mut sources);
    if let Some(existing_sources_dto) = existing_sources_dto.as_ref() {
        merge_provider_dns_resolved_from_existing(&mut sources, existing_sources_dto);
    }

    let templates_to_persist = match utils::validate_source_config_for_persist(&app_state, &sources).await {
        Ok(value) => value,
        Err(err) => {
            error!("Failed to validate source.yml {err}");
            return (axum::http::StatusCode::BAD_REQUEST, axum::Json(json!({"error": err.to_string()})))
                .into_response();
        }
    };

    if let Some(template_definition) = templates_to_persist.as_ref() {
        if let Err(err) = utils::persist_templates_config(&app_state, template_definition).await {
            error!("Failed to save template config {err}");
            return (axum::http::StatusCode::INTERNAL_SERVER_ERROR, axum::Json(json!({"error": err.to_string()})))
                .into_response();
        }
    }

    match utils::persist_source_config(&app_state, None, sources).await {
        Ok(_) => {}
        Err(err) => {
            error!("Failed to persist source.yml {err}");
            return (axum::http::StatusCode::INTERNAL_SERVER_ERROR, axum::Json(json!({"error": err.to_string()})))
                .into_response();
        }
    }

    // Reload from disk so runtime always uses fully prepared sources/mappings/templates.
    if let Err(err) = ConfigFile::load_sources(&app_state).await {
        error!("Failed to reload prepared sources after save {err}");
        return (axum::http::StatusCode::INTERNAL_SERVER_ERROR, axum::Json(json!({"error": err.to_string()})))
            .into_response();
    }

    app_state.active_provider.update_config(&app_state.app_config).await;
    axum::http::StatusCode::OK.into_response()
}

async fn get_config_api_proxy_config(
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
) -> impl IntoResponse + Send {
    let paths = app_state.app_config.paths.load();
    let api_proxy_file_path = paths.api_proxy_file_path.as_str();
    match read_api_proxy_file(api_proxy_file_path, true) {
        Ok(Some(mut api_proxy_dto)) => {
            api_proxy_dto.user = vec![];
            return axum::response::Json(api_proxy_dto).into_response();
        }
        Ok(None) => {
            error!("Failed to read api proxy config");
        }
        Err(err) => {
            error!("Failed to read api proxy config: {err}");
        }
    }
    internal_server_error!()
}

async fn save_config_api_proxy_config(
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
    axum::extract::Json(mut req_api_proxy): axum::extract::Json<ApiProxyConfigDto>,
) -> impl IntoResponse + Send {
    for server_info in &mut req_api_proxy.server {
        if !server_info.validate() {
            return (axum::http::StatusCode::BAD_REQUEST, axum::Json(json!({"error": "Invalid content"})))
                .into_response();
        }
    }

    // TODO if hot reload is on, it is loaded twice, avoid this
    // Build the updated config without mutating global state yet
    let base = app_state.app_config.api_proxy.load().as_deref().cloned().unwrap_or_default();
    let updated_api_proxy = ApiProxyConfig {
        use_user_db: req_api_proxy.use_user_db,
        server: req_api_proxy.server.iter().map(Into::into).collect(),
        ..base
    };

    let config = app_state.app_config.config.load();
    let backup_dir = config.get_backup_dir();
    let paths = app_state.app_config.paths.load();

    if let Some(err) = intern_save_config_api_proxy(
        backup_dir.as_ref(),
        &ApiProxyConfigDto::from(&updated_api_proxy),
        paths.api_proxy_file_path.as_str(),
    )
    .await
    {
        return (axum::http::StatusCode::INTERNAL_SERVER_ERROR, axum::Json(json!({"error": err.to_string()})))
            .into_response();
    }
    // Persist succeeded — now update in‑memory state
    app_state.app_config.api_proxy.store(Some(Arc::new(updated_api_proxy)));

    axum::http::StatusCode::OK.into_response()
}

async fn config(axum::extract::State(app_state): axum::extract::State<Arc<AppState>>) -> impl IntoResponse + Send {
    let paths = app_state.app_config.paths.load();
    match utils::read_app_config_dto(&paths, true, false).await {
        Ok(mut app_config) => {
            if let Err(err) = prepare_sources_batch(&mut app_config.sources, false).await {
                error!("Failed to prepare sources batch: {err}");
                internal_server_error!()
            } else if let Err(err) = prepare_users(&mut app_config, &app_state.app_config).await {
                error!("Failed to prepare users: {err}");
                internal_server_error!()
            } else {
                let runtime_sources = app_state.app_config.sources.load();
                inject_provider_dns_resolved(&mut app_config.sources, &runtime_sources);
                axum::response::Json(app_config).into_response()
            }
        }
        Err(err) => {
            error!("Failed to read config files: {err}");
            internal_server_error!()
        }
    }
}

async fn config_batch_content(
    axum::extract::Path(input_id): axum::extract::Path<u16>,
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
) -> impl IntoResponse + Send {
    if let Some(config_input) = app_state.app_config.get_input_by_id(input_id) {
        // The url is changed at this point, we need the raw url for the batch file
        if let Some(batch_url) = config_input.t_batch_url.as_ref() {
            let input_source = InputSource::from(&*config_input).with_url(batch_url.to_owned());
            return match download_text_content(
                &app_state.app_config,
                &app_state.http_client.load(),
                &input_source,
                None,
                None,
                false,
            )
            .await
            {
                Ok((content, _path)) => {
                    // Return CSV with explicit content-type
                    try_unwrap_body!(axum::response::Response::builder()
                        .status(axum::http::StatusCode::OK)
                        .header(axum::http::header::CONTENT_TYPE, "text/csv; charset=utf-8")
                        .body(content))
                }
                Err(err) => {
                    error!("Failed to read batch file: {err}");
                    internal_server_error!()
                }
            };
        }
    }
    (axum::http::StatusCode::NOT_FOUND, axum::Json(json!({"error": "Input not found or batch URL missing"})))
        .into_response()
}

pub fn v1_api_config_register(router: Router<Arc<AppState>>) -> axum::Router<Arc<AppState>> {
    router
        .route("/config", axum::routing::get(config))
        .route("/config/batchContent/{input_id}", axum::routing::get(config_batch_content))
        .route("/config/main", axum::routing::post(save_config_main))
        .route("/config/sources", axum::routing::post(save_config_sources))
        .route("/config/apiproxy", axum::routing::get(get_config_api_proxy_config))
        .route("/config/apiproxy", axum::routing::put(save_config_api_proxy_config))
}

#[cfg(test)]
mod tests {
    use super::{
        build_existing_sources_from_runtime, inject_provider_dns_resolved, merge_provider_dns_resolved_from_existing,
        strip_provider_dns_resolved,
    };
    use crate::model::{ConfigProvider, SourcesConfig};
    use indexmap::IndexMap;
    use shared::model::{ConfigProviderDto, DnsScheme, ProviderDnsDto, SourcesConfigDto};
    use std::{net::IpAddr, sync::Arc};

    #[test]
    fn inject_provider_dns_resolved_populates_runtime_snapshot() {
        let mut dto = SourcesConfigDto {
            provider: Some(vec![ConfigProviderDto {
                name: "p1".into(),
                urls: vec!["http://example.com".into()],
                dns: Some(ProviderDnsDto {
                    enabled: true,
                    schemes: Some(vec![DnsScheme::Http]),
                    ..ProviderDnsDto::default()
                }),
            }]),
            ..SourcesConfigDto::default()
        };

        let runtime_provider = Arc::new(ConfigProvider::from(&ConfigProviderDto {
            name: "p1".into(),
            urls: vec!["http://example.com".into()],
            dns: Some(ProviderDnsDto {
                enabled: true,
                schemes: Some(vec![DnsScheme::Http]),
                ..ProviderDnsDto::default()
            }),
        }));
        runtime_provider.store_resolved(
            "example.com",
            vec!["203.0.113.10".parse::<IpAddr>().expect("ip parse should work")],
        );
        let runtime_sources = SourcesConfig {
            provider: vec![runtime_provider],
            ..SourcesConfig::default()
        };

        inject_provider_dns_resolved(&mut dto, &runtime_sources);

        let resolved = dto.provider.as_ref()
            .and_then(|providers| providers.first())
            .and_then(|provider| provider.dns.as_ref())
            .and_then(|dns| dns.resolved.as_ref())
            .expect("resolved dns snapshot should be present");
        assert_eq!(
            resolved.get("example.com"),
            Some(&vec!["203.0.113.10".parse::<IpAddr>().expect("ip parse should work")])
        );
    }

    #[test]
    fn inject_provider_dns_resolved_clears_value_when_runtime_dns_disabled() {
        let mut dto = SourcesConfigDto {
            provider: Some(vec![ConfigProviderDto {
                name: "p1".into(),
                urls: vec!["http://example.com".into()],
                dns: Some(ProviderDnsDto {
                    enabled: true,
                    resolved: Some(IndexMap::from([(
                        "example.com".to_string(),
                        vec!["203.0.113.10".parse::<IpAddr>().expect("ip parse should work")],
                    )])),
                    ..ProviderDnsDto::default()
                }),
            }]),
            ..SourcesConfigDto::default()
        };

        let runtime_provider = Arc::new(ConfigProvider::from(&ConfigProviderDto {
            name: "p1".into(),
            urls: vec!["http://example.com".into()],
            dns: None,
        }));
        let runtime_sources = SourcesConfig {
            provider: vec![runtime_provider],
            ..SourcesConfig::default()
        };

        inject_provider_dns_resolved(&mut dto, &runtime_sources);

        let resolved = dto.provider.as_ref()
            .and_then(|providers| providers.first())
            .and_then(|provider| provider.dns.as_ref())
            .and_then(|dns| dns.resolved.as_ref());
        assert!(resolved.is_none(), "resolved output must be empty when runtime dns is disabled");
    }

    #[test]
    fn strip_provider_dns_resolved_removes_payload_values() {
        let mut dto = SourcesConfigDto {
            provider: Some(vec![ConfigProviderDto {
                name: "p1".into(),
                urls: vec!["http://example.com".into()],
                dns: Some(ProviderDnsDto {
                    enabled: true,
                    resolved: Some(IndexMap::from([(
                        "example.com".to_string(),
                        vec!["203.0.113.10".parse::<IpAddr>().expect("ip parse should work")],
                    )])),
                    ..ProviderDnsDto::default()
                }),
            }]),
            ..SourcesConfigDto::default()
        };

        strip_provider_dns_resolved(&mut dto);

        let resolved = dto
            .provider
            .as_ref()
            .and_then(|providers| providers.first())
            .and_then(|provider| provider.dns.as_ref())
            .and_then(|dns| dns.resolved.as_ref());
        assert!(resolved.is_none(), "resolved must be stripped from incoming payload");
    }

    #[test]
    fn merge_provider_dns_resolved_from_existing_preserves_runtime_snapshot() {
        let mut incoming = SourcesConfigDto {
            provider: Some(vec![ConfigProviderDto {
                name: "p1".into(),
                urls: vec!["http://example.com".into()],
                dns: Some(ProviderDnsDto {
                    enabled: true,
                    ..ProviderDnsDto::default()
                }),
            }]),
            ..SourcesConfigDto::default()
        };
        let existing = SourcesConfigDto {
            provider: Some(vec![ConfigProviderDto {
                name: "p1".into(),
                urls: vec!["http://example.com".into()],
                dns: Some(ProviderDnsDto {
                    enabled: true,
                    resolved: Some(IndexMap::from([(
                        "example.com".to_string(),
                        vec!["203.0.113.10".parse::<IpAddr>().expect("ip parse should work")],
                    )])),
                    ..ProviderDnsDto::default()
                }),
            }]),
            ..SourcesConfigDto::default()
        };

        merge_provider_dns_resolved_from_existing(&mut incoming, &existing);

        let resolved = incoming
            .provider
            .as_ref()
            .and_then(|providers| providers.first())
            .and_then(|provider| provider.dns.as_ref())
            .and_then(|dns| dns.resolved.as_ref())
            .expect("resolved should be merged from existing source dto");
        assert_eq!(
            resolved.get("example.com"),
            Some(&vec!["203.0.113.10".parse::<IpAddr>().expect("ip parse should work")])
        );
    }

    #[test]
    fn build_existing_sources_from_runtime_exports_provider_dns_resolved() {
        let runtime_provider = Arc::new(ConfigProvider::from(&ConfigProviderDto {
            name: "p1".into(),
            urls: vec!["http://example.com".into()],
            dns: Some(ProviderDnsDto {
                enabled: true,
                schemes: Some(vec![DnsScheme::Http]),
                ..ProviderDnsDto::default()
            }),
        }));
        runtime_provider.store_resolved(
            "example.com",
            vec!["203.0.113.10".parse::<IpAddr>().expect("ip parse should work")],
        );
        let runtime_sources = SourcesConfig {
            provider: vec![runtime_provider],
            ..SourcesConfig::default()
        };

        let exported =
            build_existing_sources_from_runtime(&runtime_sources).expect("runtime fallback dto should exist");
        let resolved = exported
            .provider
            .as_ref()
            .and_then(|providers| providers.first())
            .and_then(|provider| provider.dns.as_ref())
            .and_then(|dns| dns.resolved.as_ref())
            .expect("resolved should be present in runtime fallback dto");
        assert_eq!(
            resolved.get("example.com"),
            Some(&vec!["203.0.113.10".parse::<IpAddr>().expect("ip parse should work")])
        );
    }
}
