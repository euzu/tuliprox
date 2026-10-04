use super::{
    change_detection::{TargetCacheState, TargetChanges, TargetStatus, UpdateChanges, UpdateChangesFlags},
    create_cache, create_http_client, create_http_client_no_redirect, create_public_http_client_no_redirect,
    create_resource_http_client_no_redirect, create_resource_public_http_client_no_redirect, AppState, CancelTokens,
};
use crate::{
    api::{
        model::{load_target_into_memory_cache, recording_rule_scheduler::spawn_recording_rule_scheduler},
        tasks::{exec_config_watch, exec_scheduler},
    },
    model::{Config, ProcessTargets, SourcesConfig},
    repository::{get_geoip_path, GeoIp},
    utils::reload_logger,
};
use log::error;
use shared::{error::TuliproxError, model::WebAuthConfigDto};
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio_util::sync::CancellationToken;
use tuliprox_dvr::recording::recording_transfer::{resume_recording_worker_if_needed, spawn_recording_services};
use tuliprox_session::{provider_dns_manager::exec_provider_dns, qos_aggregation_manager::exec_qos_aggregation};

macro_rules! cancel_service {
    ($field: ident, $flag:expr, $changes:expr, $cancel_tokens:expr) => {
        if $changes.flags.contains($flag) {
            $cancel_tokens.$field.cancel();
            CancellationToken::default()
        } else {
            $cancel_tokens.$field.clone()
        }
    };
}

async fn update_target_caches(app_state: &Arc<AppState>, target_changes: Option<&HashMap<String, TargetChanges>>) {
    if let Some(target_changes) = target_changes {
        let mut to_remove = Vec::new();
        for target in target_changes.values() {
            match target.status {
                TargetStatus::Old => {
                    to_remove.push(target.name.clone());
                }
                TargetStatus::New // Normally, a new target shouldn't require any updates, but attempting to load it does no harm.
                | TargetStatus::Keep => {
                    match target.cache_status {
                        TargetCacheState::UnchangedFalse | TargetCacheState::UnchangedTrue => {} // skip this
                        TargetCacheState::ChangedToTrue => {
                            load_target_into_memory_cache(&app_state.app_config, &app_state.playlists, &target.target).await;
                        }
                        TargetCacheState::ChangedToFalse => {
                            to_remove.push(target.name.clone());
                        }
                    }
                }
            }
        }
        if !to_remove.is_empty() {
            let mut guard = app_state.playlists.data.write().await;
            for name in to_remove {
                guard.remove(&name);
            }
        }
    }
}

pub async fn update_app_state_config(app_state: &Arc<AppState>, config: Config) -> Result<(), TuliproxError> {
    let updates = app_state.set_config(config).await?;
    restart_services(app_state, &updates);
    Ok(())
}

pub async fn update_app_state_sources(
    app_state: &Arc<AppState>,
    sources: SourcesConfig,
    prevalidated_targets: Option<Arc<ProcessTargets>>,
) -> Result<(), TuliproxError> {
    let targets = if let Some(prevalidated) = prevalidated_targets {
        prevalidated
    } else {
        let targets = sources.validate_targets(Some(&app_state.playlist_updates.forced_targets.load().target_names))?;
        Arc::new(targets)
    };
    app_state.playlist_updates.forced_targets.store(targets);
    let updates = app_state.set_sources(sources)?;
    update_target_caches(app_state, updates.targets.as_ref()).await;
    restart_services(app_state, &updates);
    Ok(())
}

fn restart_services(app_state: &Arc<AppState>, changes: &UpdateChanges) {
    if !changes.modified() {
        return;
    }
    cancel_services(app_state, changes);
    start_services(app_state, changes);
}

fn cancel_services(app_state: &Arc<AppState>, changes: &UpdateChanges) {
    if !changes.modified() {
        return;
    }
    if changes.flags.contains(UpdateChangesFlags::Downloads) {
        app_state.recordings.request_worker_restart();
    }
    let cancel_tokens = app_state.cancel_tokens.load();

    let scheduler = cancel_service!(scheduler, UpdateChangesFlags::Scheduler, changes, cancel_tokens);
    let hdhomerun = cancel_service!(hdhomerun, UpdateChangesFlags::Hdhomerun, changes, cancel_tokens);
    let file_watch = cancel_service!(file_watch, UpdateChangesFlags::FileWatch, changes, cancel_tokens);
    let provider_dns = cancel_service!(provider_dns, UpdateChangesFlags::ProviderDns, changes, cancel_tokens);
    let metadata = if changes.flags.contains(UpdateChangesFlags::Metadata) {
        let token = CancellationToken::new();
        app_state.metadata_manager.rotate_cancel_token(token.clone());
        token
    } else {
        cancel_tokens.metadata.clone()
    };
    let qos_aggregation = cancel_service!(qos_aggregation, UpdateChangesFlags::QosAggregation, changes, cancel_tokens);
    let recordings = cancel_service!(recordings, UpdateChangesFlags::Downloads, changes, cancel_tokens);

    let tokens = CancelTokens {
        scheduler,
        hdhomerun,
        file_watch,
        provider_dns,
        metadata,
        qos_aggregation,
        recordings,
        hls_cache: cancel_tokens.hls_cache.clone(),
    };

    app_state.cancel_tokens.store(Arc::new(tokens));
}

fn start_services(app_state: &Arc<AppState>, changes: &UpdateChanges) {
    if !changes.modified() {
        return;
    }
    if changes.flags.contains(UpdateChangesFlags::Scheduler) {
        exec_scheduler(
            &Arc::clone(&app_state.http_clients.default.load()),
            app_state,
            &app_state.cancel_tokens.load().scheduler,
        );
    }

    if changes.flags.contains(UpdateChangesFlags::Hdhomerun) && app_state.app_config.api_proxy.load().is_some() {
        let mut infos = Vec::new();
        crate::api::main_api::start_hdhomerun(
            &app_state.app_config,
            app_state,
            &mut infos,
            &app_state.cancel_tokens.load().hdhomerun,
        );
    }

    if changes.flags.contains(UpdateChangesFlags::FileWatch) {
        exec_config_watch(app_state, &app_state.cancel_tokens.load().file_watch);
    }

    if changes.flags.contains(UpdateChangesFlags::ProviderDns) {
        exec_provider_dns(&app_state.app_config, &app_state.cancel_tokens.load().provider_dns);
    }
    if changes.flags.contains(UpdateChangesFlags::QosAggregation) {
        exec_qos_aggregation(&app_state.app_config, &app_state.cancel_tokens.load().qos_aggregation);
        let history_cfg =
            app_state.app_config.config.load().reverse_proxy.as_ref().and_then(|rp| rp.stream_history.clone());
        let connection_manager = Arc::clone(&app_state.connection_manager);
        tokio::spawn(async move {
            connection_manager.reload_history_writer(history_cfg.as_ref()).await;
        });
    }
    if changes.flags.contains(UpdateChangesFlags::Downloads) {
        spawn_recording_services(&app_state.recording_ctx(), &app_state.cancel_tokens.load().recordings);
        spawn_recording_rule_scheduler(&app_state.recording_ctx(), &app_state.cancel_tokens.load().recordings);
        let config = app_state.app_config.config.load();
        if let Some(download_cfg) = config.recording().cloned() {
            let app_state = Arc::clone(app_state);
            tokio::spawn(async move {
                for _ in 0..50 {
                    if !*app_state.recordings.worker_running.read().await {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                if let Err(err) = resume_recording_worker_if_needed(&app_state.recording_ctx(), &download_cfg).await {
                    error!("Failed to resume recordings after hot reload: {err}");
                }
            });
        }
    }
}

impl AppState {
    pub(in crate::api::model) async fn set_config(&self, config: Config) -> Result<UpdateChanges, TuliproxError> {
        let current_config = self.app_config.config.load();
        let current_web_auth =
            current_config.web_ui.as_ref().and_then(|web_ui| web_ui.auth.as_ref()).map(WebAuthConfigDto::from);
        let new_web_auth = config.web_ui.as_ref().and_then(|web_ui| web_ui.auth.as_ref()).map(WebAuthConfigDto::from);
        if current_web_auth != new_web_auth {
            return Err(TuliproxError::ConfigWebUi("web auth changes require a server restart".to_string()));
        }
        let old_storage_dir = current_config.storage_dir.clone();
        drop(current_config);
        let changes = self.detect_changes_for_config(&config);
        let config_log_level = config.log.as_ref().and_then(|log| log.log_level.clone());
        config.update_runtime();

        let use_geoip = config.is_geoip_enabled();
        let storage_dir = config.storage_dir.clone();

        self.active_users.update_config(&config);
        self.app_config.set_config(config)?;
        reload_logger(config_log_level.as_deref());
        self.active_provider.update_config(&self.app_config);
        self.hls.proxy.update_config(&self.app_config).await;
        self.update_config().await?;

        let geoip_reload_needed =
            changes.flags.contains(UpdateChangesFlags::Geoip) || (use_geoip && old_storage_dir != storage_dir);
        if geoip_reload_needed {
            let new_geoip = if use_geoip {
                let path = get_geoip_path(&storage_dir);
                let _file_lock = self.app_config.file_locks.read_lock(&path).await;
                GeoIp::load(&path).ok().map(Arc::new)
            } else {
                None
            };

            self.geoip.store(new_geoip);
        }

        shared::model::REGEX_CACHE.sweep();
        Ok(changes)
    }

    async fn update_config(&self) -> Result<(), TuliproxError> {
        // client
        let clients = &self.http_clients;
        clients.default.store(Arc::new(create_http_client(&self.app_config)?));
        clients.no_redirect.store(Arc::new(create_http_client_no_redirect(&self.app_config)?));
        clients.public_no_redirect.store(Arc::new(create_public_http_client_no_redirect(&self.app_config)?));
        clients.resource_no_redirect.store(Arc::new(create_resource_http_client_no_redirect(&self.app_config)?));
        clients
            .resource_public_no_redirect
            .store(Arc::new(create_resource_public_http_client_no_redirect(&self.app_config)?));

        // cache
        let config = self.app_config.config.load();
        let (enabled, size, cache_dir) = config
            .reverse_proxy
            .as_ref()
            .and_then(|r| r.cache.as_ref())
            .map_or((false, 0, ""), |c| (c.enabled, c.size, c.directory.as_str()));

        if let Some(cache) = self.cache.load().as_ref() {
            if enabled {
                cache.write().await.update_config(size, cache_dir);
            } else {
                self.cache.store(None);
            }
        } else {
            let cache = create_cache(&config);
            self.cache.store(cache);
        }
        Ok(())
    }

    pub(in crate::api::model) fn set_sources(&self, sources: SourcesConfig) -> Result<UpdateChanges, TuliproxError> {
        let changes = self.detect_changes_for_sources(&sources);
        // Carry over DNS caches from old providers so resolved IPs survive hot-reloads
        // without waiting for the background resolver or the persisted-file seed.
        {
            let old_sources = self.app_config.sources.load();
            for new_provider in &sources.provider {
                if let Some(old_provider) = old_sources.get_provider_by_name(&new_provider.name) {
                    if new_provider.get_dns_config().is_some_and(|cfg| cfg.enabled) {
                        for (host, ips) in old_provider.snapshot_resolved() {
                            if !ips.is_empty() && new_provider.dns_cache.ip_count(&host) == 0 {
                                new_provider.dns_cache.store_resolved(&host, ips);
                            }
                        }
                    }
                }
            }
        }
        self.app_config.set_sources(sources)?;
        self.active_provider.update_config(&self.app_config);

        shared::model::REGEX_CACHE.sweep();
        Ok(changes)
    }
}
