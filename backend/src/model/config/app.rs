use crate::api::model::TransportStreamBuffer;
use crate::model::{ApiProxyConfig, ApiProxyServerInfo, Config, ConfigInput, ConfigInputOptions, ConfigTarget, CustomStreamResponse, HdHomeRunConfig, Mappings, ProxyUserCredentials, SourcesConfig, TargetOutput};
use crate::utils;
use arc_swap::access::Access;
use arc_swap::{ArcSwap, ArcSwapOption};
use log::{error, warn};
use rand::Rng;
use shared::info_err_res;
use shared::error::{TuliproxError, TuliproxErrorKind};
use shared::model::ConfigPaths;
use std::borrow::Cow;
use std::collections::HashSet;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;

const CHANNEL_UNAVAILABLE: &str = "channel_unavailable.ts";
const USER_CONNECTIONS_EXHAUSTED: &str = "user_connections_exhausted.ts";
const PROVIDER_CONNECTIONS_EXHAUSTED: &str = "provider_connections_exhausted.ts";
const USER_ACCOUNT_EXPIRED: &str = "user_account_expired.ts";

fn generate_secret() -> [u8; 32] {
    let mut rng = rand::rng();
    let mut secret = [0u8; 32];
    rng.fill(&mut secret);
    secret
}

#[derive(Debug, Clone)]
pub struct AppConfig {
    pub config: Arc<ArcSwap<Config>>,
    pub sources: Arc<ArcSwap<SourcesConfig>>,
    pub hdhomerun: Arc<ArcSwapOption<HdHomeRunConfig>>,
    pub api_proxy: Arc<ArcSwapOption<ApiProxyConfig>>,
    pub file_locks: Arc<utils::FileLockManager>,
    pub paths: Arc<ArcSwap<ConfigPaths>>,
    pub custom_stream_response: Arc<ArcSwapOption<CustomStreamResponse>>,
    pub access_token_secret: [u8; 32],
    pub encrypt_secret: [u8; 16],
}

impl AppConfig {
    pub fn set_config(&self, config: Config) -> Result<(), TuliproxError> {
        self.config.store(Arc::new(config));
        self.prepare_paths();
        Ok(())
    }

    pub fn set_sources(&self, sources: SourcesConfig) -> Result<(), TuliproxError> {
        self.sources.store(Arc::new(sources));
        self.prepare_sources()?;
        Ok(())
    }

    pub fn set_api_proxy(&self, api_proxy: ApiProxyConfig) -> Result<(), TuliproxError> {
        self.api_proxy.store(Some(Arc::new(api_proxy)));
        self.check_target_user()
    }

    pub fn set_mappings(&self, mapping_path: &str, mappings_cfg: &Mappings) {
        self.set_mapping_path(Some(mapping_path));
        let sources = <Arc<ArcSwap<SourcesConfig>> as Access<SourcesConfig>>::load(&self.sources);
        for source in &sources.sources {
            for target in &source.targets {
                if let Some(mapping_ids) = &target.mapping_ids {
                    let mut target_mappings = Vec::with_capacity(128);
                    for mapping_id in mapping_ids {
                        let mapping = mappings_cfg.get_mapping(mapping_id);
                        if let Some(mappings) = mapping {
                            target_mappings.push(mappings);
                        }
                    }
                    target.mapping.store(if target_mappings.is_empty() { None } else { Some(Arc::new(target_mappings)) });
                }
            }
        }
    }

    fn check_username(&self, output_username: Option<&str>, target_name: &str) -> Result<(), TuliproxError> {
        if let Some(username) = output_username {
            if let Some((_, config_target)) = self.get_target_for_username(username) {
                if config_target.name != target_name {
                    return info_err_res!("User:{username} does not belong to target: {}", target_name);
                }
            } else {
                return info_err_res!("User: {username} does not exist");
            }
            Ok(())
        } else {
            Ok(())
        }
    }
    fn check_target_user(&self) -> Result<(), TuliproxError> {
        let check_homerun = {
            let config = <Arc<ArcSwap<Config>> as Access<Config>>::load(&self.config);
            self.hdhomerun.store(config.hdhomerun.as_ref().map(|h| Arc::new(h.clone())));
            config.hdhomerun.as_ref().is_some_and(|h| h.enabled)
        };
        let sources = <Arc<ArcSwap<SourcesConfig>> as Access<SourcesConfig>>::load(&self.sources);
        for source in &sources.sources {
            for target in &source.targets {
                for output in &target.output {
                    match output {
                        TargetOutput::Xtream(_) | TargetOutput::M3u(_) => {}
                        TargetOutput::Strm(strm_output) => {
                            self.check_username(strm_output.username.as_deref(), &target.name)?;
                        }
                        TargetOutput::HdHomeRun(hdhomerun_output) => {
                            if check_homerun {
                                let hdhr_name = &hdhomerun_output.device;
                                self.check_username(Some(&hdhomerun_output.username), &target.name)?;
                                if let Some(old_hdhomerun) = self.hdhomerun.load().clone() {
                                    let mut hdhomerun = (*old_hdhomerun).clone();
                                    for device in &mut hdhomerun.devices {
                                        if &device.name == hdhr_name {
                                            device.t_username.clone_from(&hdhomerun_output.username);
                                            device.t_enabled = true;
                                        }
                                    }
                                    self.hdhomerun.store(Some(Arc::new(hdhomerun)));
                                }
                            }
                        }
                    }
                }
            }
        }

        let guard = self.hdhomerun.load();
        if let Some(hdhomerun) = &*guard {
            for device in &hdhomerun.devices {
                if !device.t_enabled {
                    warn!("HdHomeRun device '{}' has no username and will be disabled", device.name);
                }
            }
        }
        Ok(())
    }

    pub fn is_reverse_proxy_resource_rewrite_enabled(&self) -> bool {
        let config = <Arc<ArcSwap<Config>> as Access<Config>>::load(&self.config);
        config.reverse_proxy.as_ref().is_none_or(|r| !r.resource_rewrite_disabled)
    }

    pub fn get_reverse_proxy_rewrite_secret(&self) -> Option<[u8; 16]> {
        let config = <Arc<ArcSwap<Config>> as Access<Config>>::load(&self.config);
        config.reverse_proxy.as_ref().map(|r| r.rewrite_secret)
    }

    fn intern_get_target_for_user(&self, user_target: Option<(ProxyUserCredentials, String)>) -> Option<(ProxyUserCredentials, Arc<ConfigTarget>)> {
        match user_target {
            Some((user, target_name)) => {
                let sources = <Arc<ArcSwap<SourcesConfig>> as Access<SourcesConfig>>::load(&self.sources);
                for source in &sources.sources {
                    for target in &source.targets {
                        if target_name.eq_ignore_ascii_case(&target.name) {
                            return Some((user, Arc::clone(target)));
                        }
                    }
                }
                None
            }
            None => None
        }
    }

    pub fn get_inputs_for_target(&self, target_name: &str) -> Option<Vec<Arc<ConfigInput>>> {
        let sources = <Arc<ArcSwap<SourcesConfig>> as Access<SourcesConfig>>::load(&self.sources);
        if let Some(inputs) = sources.get_source_inputs_by_target_by_name(target_name) {
            let result: Vec<Arc<ConfigInput>> = sources
                .inputs
                .iter()
                .filter(|s| inputs.contains(&s.name))
                .map(Arc::clone)
                .collect();
            if !result.is_empty() {
                return Some(result)
            }
        }
        None
    }

    pub fn get_target_for_username(&self, username: &str) -> Option<(ProxyUserCredentials, Arc<ConfigTarget>)> {
        if let Some(credentials) = self.get_user_credentials(username) {
            return self.api_proxy.load().as_ref()
                .and_then(|api_proxy| self.intern_get_target_for_user(api_proxy.get_target_name(&credentials.username, &credentials.password)));
        }
        None
    }

    pub fn get_target_for_user(&self, username: &str, password: &str) -> Option<(ProxyUserCredentials, Arc<ConfigTarget>)> {
        self.api_proxy.load().as_ref().and_then(|api_proxy| self.intern_get_target_for_user(api_proxy.get_target_name(username, password)))
    }

    pub fn get_target_for_user_by_token(&self, token: &str) -> Option<(ProxyUserCredentials, Arc<ConfigTarget>)> {
        self.api_proxy.load().as_ref().as_ref().and_then(|api_proxy| self.intern_get_target_for_user(api_proxy.get_target_name_by_token(token)))
    }

    pub fn get_user_credentials(&self, username: &str) -> Option<ProxyUserCredentials> {
        self.api_proxy.load().as_ref().as_ref().and_then(|api_proxy| api_proxy.get_user_credentials(username))
    }

    pub fn get_input_by_name(&self, input_name: &Arc<str>) -> Option<Arc<ConfigInput>> {
        let sources = <Arc<ArcSwap<SourcesConfig>> as Access<SourcesConfig>>::load(&self.sources);
        for input in &sources.inputs {
            if &input.name == input_name {
                return Some(Arc::clone(input));
            }
        }
        None
    }

    pub fn get_input_options_by_name(&self, input_name: &Arc<str>) -> Option<ConfigInputOptions> {
        let sources = <Arc<ArcSwap<SourcesConfig>> as Access<SourcesConfig>>::load(&self.sources);
        for input in &sources.inputs {
            if &input.name == input_name {
                return input.options.clone();
            }
        }
        None
    }

    pub fn get_input_by_id(&self, input_id: u16) -> Option<Arc<ConfigInput>> {
        let sources = <Arc<ArcSwap<SourcesConfig>> as Access<SourcesConfig>>::load(&self.sources);
            for input in &sources.inputs {
                if input.id == input_id {
                    return Some(Arc::clone(input));
                }
                if let Some(aliases) = input.aliases.as_ref() {
                    for alias in aliases {
                        if alias.id == input_id {
                            return Some(Arc::new(input.as_input(alias)));
                        }
                    }
                }
        }
        None
    }

    pub fn get_target_by_id(&self, target_id: u16) -> Option<Arc<ConfigTarget>> {
        let sources = <Arc<ArcSwap<SourcesConfig>> as Access<SourcesConfig>>::load(&self.sources);
        sources.get_target_by_id(target_id)
    }

    fn check_unique_input_names(&self) -> Result<(), TuliproxError> {
        let mut seen_names: HashSet<String> = HashSet::new();
        let sources = <Arc<ArcSwap<SourcesConfig>> as Access<SourcesConfig>>::load(&self.sources);
        for input in &sources.inputs {
            let input_name = input.name.trim();
            if input_name.is_empty() {
                return info_err_res!("input name required");
            }
            if seen_names.contains(input_name) {
                return info_err_res!("input names should be unique: {}", input_name);
            }
            seen_names.insert(input_name.to_string());
            if let Some(aliases) = &input.aliases {
                for alias in aliases {
                    let input_name = alias.name.trim().to_string();
                    if input_name.is_empty() {
                        return info_err_res!("input name required");
                    }
                    if seen_names.contains(&input_name) {
                        return info_err_res!("input and alias names should be unique: {}", input_name);
                    }
                    seen_names.insert(input_name.clone());
                }
            }
        }

        Ok(())
    }


    fn check_scheduled_targets(&self, target_names: &HashSet<Cow<str>>) -> Result<(), TuliproxError> {
        let config = <Arc<ArcSwap<Config>> as Access<Config>>::load(&self.config);
        if let Some(schedules) = &config.schedules {
            for schedule in schedules {
                if let Some(targets) = &schedule.targets {
                    for target_name in targets {
                        if !target_names.contains(target_name.as_str()) {
                            return info_err_res!("Unknown target name in scheduler: {}", target_name);
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /**
    *  if `include_computed` set to true for `app_state`
    */
    pub fn prepare(&mut self, include_computed: bool) -> Result<(), TuliproxError> {
        if include_computed {
            self.access_token_secret = generate_secret();
            self.encrypt_secret = <&[u8] as TryInto<[u8; 16]>>::try_into(&generate_secret()[0..16]).map_err(|err| TuliproxError::new(TuliproxErrorKind::Info, err.to_string()))?;
            self.prepare_paths();
        } else {
            self.prepare_mapping_path();
        }

        self.prepare_sources()?;

        Ok(())
    }

    fn prepare_sources(&self) -> Result<(), TuliproxError> {
        let sources = <Arc<ArcSwap<SourcesConfig>> as Access<SourcesConfig>>::load(&self.sources);
        let target_names = sources.get_unique_target_names();
        self.check_scheduled_targets(&target_names)?;
        self.check_unique_input_names()?;
        Ok(())
    }

    fn set_mapping_path(&self, mapping_path: Option<&str>) {
        let paths = self.paths.load_full();
        let mut new_paths = paths.as_ref().clone();
        let old_mapping_file_path = new_paths.mapping_file_path.as_deref();
        if old_mapping_file_path != mapping_path {
            new_paths.mapping_file_path = mapping_path.map(ToString::to_string);
            self.paths.store(Arc::new(new_paths));
        }
    }

    fn prepare_mapping_path(&self) {
        let config = <Arc<ArcSwap<Config>> as Access<Config>>::load(&self.config);
        self.set_mapping_path(config.mapping_path.as_deref());
    }

    fn prepare_paths(&self) {
        self.prepare_mapping_path();
        self.prepare_custom_stream_response();
    }

    fn prepare_custom_stream_response(&self) {
        let config = <Arc<ArcSwap<Config>> as Access<Config>>::load(&self.config);
        if let Some(custom_stream_response_path) = config.custom_stream_response_path.as_ref() {
            fn load_and_set_file(file_path: &Path) -> Option<TransportStreamBuffer> {
                if file_path.exists() {
                    // Enforce maximum file size (10 MB)
                    if let Ok(meta) = std::fs::metadata(file_path) {
                        const MAX_RESPONSE_SIZE: u64 = 10 * 1024 * 1024;
                        if meta.len() > MAX_RESPONSE_SIZE {
                            error!("Custom stream response file too large ({} bytes): {}",
                                   meta.len(), file_path.display());
                            return None;
                        }
                    }
                    // Quick MPEG-TS sync-byte check (0x47)
                    if let Ok(mut f) = File::open(file_path) {
                        let mut buf = [0u8; 1];
                        if f.read_exact(&mut buf).is_err() || buf[0] != 0x47 {
                            error!("Invalid MPEG-TS file: {}", file_path.display());
                            return None;
                        }
                    }

                    match utils::read_file_as_bytes(&PathBuf::from(&file_path)) {
                        Ok(data) => Some(TransportStreamBuffer::new(data)),
                        Err(err) => {
                            error!("Failed to load a resource file: {} {err}", file_path.display());
                            None
                        }
                    }
                } else {
                    None
                }
            }

            let path = PathBuf::from(custom_stream_response_path);
            let path = utils::make_path_absolute(&path, &config.working_dir);

            let paths = self.paths.load_full();
            let mut new_paths = paths.as_ref().clone();
            new_paths.custom_stream_response_path = Some(path.to_string_lossy().to_string());
            self.paths.store(Arc::new(new_paths));

            let channel_unavailable = load_and_set_file(&path.join(CHANNEL_UNAVAILABLE));
            let user_connections_exhausted = load_and_set_file(&path.join(USER_CONNECTIONS_EXHAUSTED));
            let provider_connections_exhausted = load_and_set_file(&path.join(PROVIDER_CONNECTIONS_EXHAUSTED));
            let user_account_expired = load_and_set_file(&path.join(USER_ACCOUNT_EXPIRED));
            self.custom_stream_response.store(Some(Arc::new(CustomStreamResponse {
                channel_unavailable,
                user_connections_exhausted,
                provider_connections_exhausted,
                user_account_expired,
            })));
        }
    }


    /// # Panics
    ///
    /// Will panic if default server invalid
    pub fn get_server_info(&self, server_info_name: &str) -> ApiProxyServerInfo {
        let guard = self.api_proxy.load();
        if let Ok(api_proxy) = guard.as_ref().ok_or_else(|| {
            TuliproxError::new(TuliproxErrorKind::Info, "API proxy config not loaded".to_string())
        }) {
            let server_info_list = api_proxy.server.clone();
            server_info_list.iter().find(|c| c.name.eq(server_info_name))
                .map_or_else(|| server_info_list.first().unwrap().clone(), Clone::clone)
        } else {
            panic!("ApiProxyServer info not found");
        }
    }

    pub fn get_user_server_info(&self, user: &ProxyUserCredentials) -> ApiProxyServerInfo {
        let server_info_name = user.server.as_ref().map_or("default", |server_name| server_name.as_str());
        self.get_server_info(server_info_name)
    }
}


