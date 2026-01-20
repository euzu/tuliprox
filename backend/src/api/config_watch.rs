use crate::api::config_file::ConfigFile;
use crate::api::model::{AppState, EventMessage};
use crate::model::{Config, SourcesConfig};
use crate::utils;
use crate::utils::is_directory;
use arc_swap::access::Access;
use arc_swap::ArcSwap;
use log::{error, info};
use notify::event::{AccessKind, AccessMode};
use notify::{recommended_watcher, EventKind, RecursiveMode, Watcher};
use shared::error::{TuliproxError, TuliproxErrorKind};
use shared::model::ConfigPaths;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

#[allow(clippy::too_many_lines)]
fn start_config_watch(
    app_state: &Arc<AppState>,
    cancel_token: &CancellationToken,
) -> Result<(), TuliproxError> {
    // let (tx, mut rx) = mpsc::channel::<notify::Result<Event>>(100);
    let (std_tx, std_rx) = std::sync::mpsc::channel();
    let (tx, mut rx) = tokio::sync::mpsc::channel(100);

    let paths =
        <Arc<ArcSwap<ConfigPaths>> as Access<ConfigPaths>>::load(&app_state.app_config.paths);
    let mapping_file_path = paths
        .mapping_file_path
        .as_ref()
        .map_or_else(String::new, ToString::to_string);
    let files = get_watch_files(app_state, &paths, mapping_file_path.as_str());
    //
    // // Add a path to be watched. All files and directories at that path and
    // // below will be monitored for changes.
    let path = Path::new(paths.config_path.as_str());
    let recursive_mode = if !mapping_file_path.is_empty() && utils::is_directory(&mapping_file_path)
    {
        RecursiveMode::Recursive
    } else {
        RecursiveMode::NonRecursive
    };

    std::thread::spawn({
        let tx = tx.clone();
        move || {
            for res in std_rx {
                let _ = tx.blocking_send(res);
            }
            //debug!("Stopping config watch channel thread");
        }
    });

    let mut watcher = recommended_watcher(std_tx).map_err(|err| {
        TuliproxError::new(
            TuliproxErrorKind::Info,
            format!("Failed to init config file watcher {err}"),
        )
    })?;
    watcher.watch(path, recursive_mode).map_err(|err| {
        TuliproxError::new(
            TuliproxErrorKind::Info,
            format!("Failed to start config file watcher {err}"),
        )
    })?;
    info!("Watching config file changes {}", path.display());

    let event_manager = Arc::clone(&app_state.event_manager);
    let cancel = cancel_token.clone();
    let watcher_app_state = Arc::clone(app_state);

    let handle_error = move |err: TuliproxError, path: &Path| {
        let msg = format!("Failed to reload config file {}: {err}", path.display());
        error!("{msg}");
        event_manager.send_event(EventMessage::ServerError(msg));
    };

    tokio::spawn(async move {
        info!("Configuration file watcher started.");

        let _keep_watcher_alive = watcher;

        let mut debounce_timer =
            Box::pin(tokio::time::sleep(tokio::time::Duration::from_millis(0)));
        let mut timer_active = false;
        let mut pending_configs: HashMap<ConfigFile, PathBuf> = HashMap::new();

        loop {
            tokio::select! {
            biased;

            () = cancel.cancelled() => {
                info!("Cancellation received, shutting down watcher task.");
                break;
            }

            Some(res) = rx.recv() => {
                match res {
                    Ok(event) => {
                        // Config updates may be written atomically (temp file + rename), which
                        // doesn't always emit a Close(Write) event for the destination file.
                        // Treat rename/modify/create/remove events as reload triggers as well.
                        let is_write_event = matches!(
                            event.kind,
                            EventKind::Access(AccessKind::Close(AccessMode::Write))
                                | EventKind::Modify(_)
                                | EventKind::Create(_)
                                | EventKind::Remove(_)
                        );
                        if is_write_event {
                            let mut trigger = false;
                            for path in event.paths {
                                let mut resolved = None;
                                if let Some((config_file, _is_dir)) = files.get(&path) {
                                    resolved = Some(*config_file);
                                } else if recursive_mode == RecursiveMode::Recursive && path.extension().is_some_and(|ext| ext == "yml") {
                                    for (key, (config_file, is_dir)) in &files {
                                        if *is_dir && path.starts_with(key) {
                                            resolved = Some(*config_file);
                                            break;
                                        }
                                    }
                                }

                                if let Some(config_file) = resolved {
                                    // Consolidate updates
                                    let effective = if config_file == ConfigFile::SourceFile { ConfigFile::Sources } else { config_file };
                                    pending_configs.insert(effective, path);
                                    trigger = true;
                                }
                            }

                            if trigger {
                                debounce_timer = Box::pin(tokio::time::sleep(tokio::time::Duration::from_millis(500)));
                                timer_active = true;
                            }
                        }
                    }
                    Err(e) => error!("watch error: {e:?}"),
                }
            }

            () = &mut debounce_timer, if timer_active => {
                timer_active = false;
                if !pending_configs.is_empty() {
                    for (config_file, path) in pending_configs.drain() {
                        if let Err(err) = config_file.reload(&path, &watcher_app_state).await {
                           handle_error(err, &path);
                        }
                    }
                }
            }

            else => break, // Channel closed
            }
        }
        info!("Configuration file watcher terminated.");
    });

    Ok(())
}

fn get_watch_files(
    app_state: &Arc<AppState>,
    paths: &ConfigPaths,
    mapping_file_path: &str,
) -> HashMap<PathBuf, (ConfigFile, bool)> {
    let sources =
        <Arc<ArcSwap<SourcesConfig>> as Access<SourcesConfig>>::load(&app_state.app_config.sources);
    let input_files_paths = sources.get_input_files();
    let mut files = HashMap::new();
    [
        (paths.config_file_path.as_str(), ConfigFile::Config),
        (paths.api_proxy_file_path.as_str(), ConfigFile::ApiProxy),
        (mapping_file_path, ConfigFile::Mapping),
        (paths.sources_file_path.as_str(), ConfigFile::Sources),
    ]
    .into_iter()
    .filter(|(path, _)| !path.is_empty())
    .for_each(|(path, config_file)| {
        files.insert(PathBuf::from(path), (config_file, is_directory(path)));
    });
    for path in input_files_paths {
        files.insert(path, (ConfigFile::SourceFile, false));
    }
    files
}

pub fn exec_config_watch(app_state: &Arc<AppState>, cancel: &CancellationToken) {
    let hot_reload = {
        let config = <Arc<ArcSwap<Config>> as Access<Config>>::load(&app_state.app_config.config);
        config.config_hot_reload
    };

    if hot_reload {
        if let Err(err) = start_config_watch(app_state, cancel) {
            error!("Failed to start config watch: {err}");
        }
    }
}
