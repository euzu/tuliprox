use crate::{
    model::{
        ApiProxyConfig, ApiProxyServerInfo, CompiledMappings, CompiledTargetMappings, Config, ConfigInput,
        ConfigInputOptions, ConfigTarget, CustomStreamResponse, GracePeriodOptions, HdHomeRunConfig, HdHomeRunFlags,
        MediaToolCapabilities, ProxyUserCredentials, ReverseProxyDisabledHeaderConfig, SourcesConfig, TargetOutput,
    },
    utils,
};
use arc_swap::{ArcSwap, ArcSwapOption};
use log::{error, warn};
use rand::Rng;
use shared::{
    defaults::{
        CHANNEL_UNAVAILABLE, HLS_SESSION_OR_LEASE_EXPIRED, LOW_PRIORITY_PREEMPTED, PANEL_API_PROVISIONING,
        PANEL_API_PROVISIONING_HLS_SEGMENT_COUNT, PANEL_API_PROVISIONING_HLS_SEGMENT_PREFIX,
        PROVIDER_CONNECTIONS_EXHAUSTED, USER_ACCOUNT_EXPIRED, USER_CONNECTIONS_EXHAUSTED,
    },
    error::TuliproxError,
    model::{ConfigPaths, GeoIpUnavailablePolicy},
};
use std::{
    borrow::Cow,
    collections::HashSet,
    fs::File,
    io::Read,
    path::{Path, PathBuf},
    sync::Arc,
};
use tuliprox_mpegts::transport_stream_buffer::TransportStreamBuffer;

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
    pub media_tools: Arc<MediaToolCapabilities>,
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

    pub fn set_mappings(&self, mapping_path: &str, mappings_cfg: &CompiledMappings) {
        self.set_mapping_path(Some(mapping_path));
        let sources = self.sources.load();

        // Warn only if mappings were actually loaded; target mapping stores still need updating below.
        if !mappings_cfg.mappings.is_empty() {
            // Collect all mapping_ids referenced by targets
            let mut referenced_ids: HashSet<&str> = HashSet::new();
            for source in &sources.sources {
                for target in &source.targets {
                    if let Some(ref ids) = target.mapping_ids {
                        for id in ids {
                            referenced_ids.insert(id.as_str());
                        }
                    }
                }
            }

            // Warn about loaded-but-unreferenced mappings (may be intentional template/experiment files)
            for mapping in &mappings_cfg.mappings {
                if !referenced_ids.contains(mapping.id.as_str()) {
                    warn!(
                        "Mapping '{}' is loaded but not referenced by any target; it has no effect unless added to a target mapping list",
                        mapping.id
                    );
                }
            }

            // Warn about mappings that have neither mapper nor counter
            for mapping in &mappings_cfg.mappings {
                let has_mapper = !mapping.rules.is_empty();
                let has_counter = !mapping.counters.is_empty();
                if !has_mapper && !has_counter {
                    warn!("Mapping '{}' has neither mapper nor counter and has no effect", mapping.id);
                }
            }
        }

        for source in &sources.sources {
            for target in &source.targets {
                if let Some(mapping_ids) = &target.mapping_ids {
                    let mut target_mappings = Vec::with_capacity(128);
                    for mapping_id in mapping_ids {
                        let mapping = mappings_cfg.get_mapping(mapping_id);
                        if let Some(mappings) = mapping {
                            target_mappings.push(mappings);
                        } else {
                            warn!(
                                "Target '{}' references unknown mapping '{}'; the mapping will not be applied",
                                target.name, mapping_id
                            );
                        }
                    }
                    target.mapping.store(if target_mappings.is_empty() {
                        None
                    } else {
                        Some(Arc::new(CompiledTargetMappings::new(target_mappings)))
                    });
                }
            }
        }
    }

    fn check_username(&self, output_username: Option<&str>, target_name: &str) -> Result<(), TuliproxError> {
        if let Some(username) = output_username {
            if let Some((_, config_target)) = self.get_target_for_username(username) {
                if config_target.name != target_name {
                    return Err(TuliproxError::Config(format!(
                        "User:{username} does not belong to target: {target_name}"
                    )));
                }
            } else {
                return Err(TuliproxError::Config(format!("User: {username} does not exist")));
            }
            Ok(())
        } else {
            Ok(())
        }
    }
    fn check_target_user(&self) -> Result<(), TuliproxError> {
        let check_homerun = {
            let config = self.config.load();
            self.hdhomerun.store(config.hdhomerun.as_ref().map(|h| Arc::new(h.clone())));
            config.hdhomerun.as_ref().is_some_and(|h| h.flags.contains(HdHomeRunFlags::Enabled))
        };
        let sources = self.sources.load();
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
        let config = self.config.load();
        config.reverse_proxy.as_ref().is_none_or(|r| !r.resource_rewrite_disabled)
    }

    /// The secret used to rewrite resource URLs, falling back to the configured
    /// encrypt secret. Derived entirely from this configuration, so callers do
    /// not need the server state to obtain it.
    pub fn get_encrypt_secret(&self) -> [u8; 16] {
        self.get_reverse_proxy_rewrite_secret().unwrap_or(self.encrypt_secret)
    }

    pub fn get_reverse_proxy_rewrite_secret(&self) -> Option<[u8; 16]> {
        let config = self.config.load();
        config.reverse_proxy.as_ref().map(|r| r.rewrite_secret)
    }

    fn intern_get_target_for_user(
        &self,
        user_target: Option<(Arc<ProxyUserCredentials>, String)>,
    ) -> Option<(Arc<ProxyUserCredentials>, Arc<ConfigTarget>)> {
        match user_target {
            Some((user, target_name)) => {
                let sources = self.sources.load();
                for source in &sources.sources {
                    for target in &source.targets {
                        if target_name.eq_ignore_ascii_case(&target.name) {
                            return Some((Arc::clone(&user), Arc::clone(target)));
                        }
                    }
                }
                None
            }
            None => None,
        }
    }

    pub fn get_inputs_for_target(&self, target_name: &str) -> Option<Vec<Arc<ConfigInput>>> {
        let sources = self.sources.load();
        if let Some(inputs) = sources.get_source_inputs_by_target_by_name(target_name) {
            let result: Vec<Arc<ConfigInput>> =
                sources.inputs.iter().filter(|s| inputs.contains(&s.name)).map(Arc::clone).collect();
            if !result.is_empty() {
                return Some(result);
            }
        }
        None
    }

    pub fn get_target_for_username(&self, username: &str) -> Option<(Arc<ProxyUserCredentials>, Arc<ConfigTarget>)> {
        if let Some(credentials) = self.get_user_credentials(username) {
            return self.api_proxy.load().as_ref().and_then(|api_proxy| {
                self.intern_get_target_for_user(api_proxy.get_target_name(&credentials.username, &credentials.password))
            });
        }
        None
    }

    pub fn get_target_for_user(
        &self,
        username: &str,
        password: &str,
    ) -> Option<(Arc<ProxyUserCredentials>, Arc<ConfigTarget>)> {
        self.api_proxy
            .load()
            .as_ref()
            .and_then(|api_proxy| self.intern_get_target_for_user(api_proxy.get_target_name(username, password)))
    }

    pub fn get_target_for_user_by_token(&self, token: &str) -> Option<(Arc<ProxyUserCredentials>, Arc<ConfigTarget>)> {
        self.api_proxy
            .load()
            .as_ref()
            .and_then(|api_proxy| self.intern_get_target_for_user(api_proxy.get_target_name_by_token(token)))
    }

    pub fn get_user_credentials(&self, username: &str) -> Option<Arc<ProxyUserCredentials>> {
        self.api_proxy.load().as_ref().as_ref().and_then(|api_proxy| api_proxy.get_user_credentials(username))
    }

    pub fn get_auth_error_status(&self) -> axum::http::StatusCode {
        let status = self
            .api_proxy
            .load()
            .as_ref()
            .map_or(shared::defaults::default_auth_error_status(), |api_proxy| api_proxy.auth_error_status);
        axum::http::StatusCode::from_u16(status).unwrap_or(axum::http::StatusCode::FORBIDDEN)
    }

    pub fn get_input_by_name(&self, input_name: &Arc<str>) -> Option<Arc<ConfigInput>> {
        let sources = self.sources.load();
        for input in &sources.inputs {
            if &input.name == input_name {
                return Some(Arc::clone(input));
            }
        }
        None
    }

    pub fn get_input_options_by_name(&self, input_name: &Arc<str>) -> Option<ConfigInputOptions> {
        let sources = self.sources.load();
        for input in &sources.inputs {
            if &input.name == input_name {
                return input.options.clone();
            }
        }
        None
    }

    pub fn get_input_by_id(&self, input_id: u16) -> Option<Arc<ConfigInput>> {
        let sources = self.sources.load();
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
        let sources = self.sources.load();
        sources.get_target_by_id(target_id)
    }

    fn check_unique_input_names(&self) -> Result<(), TuliproxError> {
        let mut seen_names: HashSet<String> = HashSet::new();
        let sources = self.sources.load();
        for input in &sources.inputs {
            let input_name = input.name.trim();
            if input_name.is_empty() {
                return Err(TuliproxError::Config("input name required".to_string()));
            }
            if seen_names.contains(input_name) {
                return Err(TuliproxError::Config(format!("input names should be unique: {input_name}")));
            }
            seen_names.insert(input_name.to_string());
            if let Some(aliases) = &input.aliases {
                for alias in aliases {
                    let input_name = alias.name.trim().to_string();
                    if input_name.is_empty() {
                        return Err(TuliproxError::Config("input name required".to_string()));
                    }
                    if seen_names.contains(&input_name) {
                        return Err(TuliproxError::Config(format!(
                            "input and alias names should be unique: {input_name}"
                        )));
                    }
                    seen_names.insert(input_name.clone());
                }
            }
        }

        Ok(())
    }

    fn check_scheduled_targets(&self, target_names: &HashSet<Cow<str>>) -> Result<(), TuliproxError> {
        let config = self.config.load();
        if let Some(schedules) = &config.schedules {
            for schedule in schedules {
                if let Some(targets) = &schedule.targets {
                    for target_name in targets {
                        if !target_names.contains(target_name.as_str()) {
                            return Err(TuliproxError::Config(format!(
                                "Unknown target name in scheduler: {target_name}"
                            )));
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
            self.encrypt_secret = <&[u8] as TryInto<[u8; 16]>>::try_into(&generate_secret()[0..16])
                .map_err(|err| TuliproxError::Crypto(err.to_string()))?;
            self.prepare_paths();
        } else {
            self.prepare_mapping_path();
            self.prepare_template_path();
            self.prepare_custom_stream_response();
        }

        self.prepare_sources()?;

        Ok(())
    }

    fn prepare_sources(&self) -> Result<(), TuliproxError> {
        let sources = self.sources.load();
        let target_names = sources.get_unique_target_names();
        self.check_scheduled_targets(&target_names)?;
        self.check_unique_input_names()?;
        Ok(())
    }

    fn set_mapping_path(&self, mapping_path: Option<&str>) {
        self.set_optional_path(
            mapping_path,
            utils::resolve_mapping_file_path,
            |paths| &paths.mapping_file_path,
            |paths, value| paths.mapping_file_path = value,
        );
    }

    fn set_template_path(&self, template_path: Option<&str>) {
        self.set_optional_path(
            template_path,
            utils::resolve_template_file_path,
            |paths| &paths.template_file_path,
            |paths, value| paths.template_file_path = value,
        );
    }

    /// Shared body of `set_mapping_path` / `set_template_path`.
    /// Resolves a candidate file path, compares it with the current value
    /// (read via `getter`), and only stores a new `ConfigPaths` when the
    /// value actually changed.
    fn set_optional_path<R, G, S>(&self, requested: Option<&str>, resolver: R, getter: G, setter: S)
    where
        R: Fn(&str, Option<&str>) -> String,
        G: Fn(&ConfigPaths) -> &Option<String>,
        S: Fn(&mut ConfigPaths, Option<String>),
    {
        let paths_guard = self.paths.load();
        let new_path = Some(resolver(paths_guard.config_path.as_str(), requested));
        if getter(&paths_guard).as_deref() != new_path.as_deref() {
            let mut new_paths = (**paths_guard).clone();
            setter(&mut new_paths, new_path);
            self.paths.store(Arc::new(new_paths));
        }
    }

    fn prepare_mapping_path(&self) {
        let config = self.config.load();
        self.set_mapping_path(config.mapping_path.as_deref());
    }

    fn prepare_template_path(&self) {
        let config = self.config.load();
        self.set_template_path(config.template_path.as_deref());
    }

    fn prepare_paths(&self) {
        self.prepare_mapping_path();
        self.prepare_template_path();
        self.prepare_custom_stream_response();
    }

    fn prepare_custom_stream_response(&self) {
        let config = self.config.load();
        if let Some(custom_stream_response_path) = config.custom_stream_response_path.as_ref() {
            fn load_and_set_file(file_path: &Path) -> Option<TransportStreamBuffer> {
                if file_path.exists() {
                    // Enforce maximum file size (10 MB)
                    if let Ok(meta) = std::fs::metadata(file_path) {
                        const MAX_RESPONSE_SIZE: u64 = 10 * 1024 * 1024;
                        if meta.len() > MAX_RESPONSE_SIZE {
                            error!(
                                "Custom stream response file too large ({} bytes): {}",
                                meta.len(),
                                file_path.display()
                            );
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
            let home_path = self.paths.load().home_path.clone();
            let path = utils::make_path_absolute(&path, home_path.as_str());

            let paths = self.paths.load_full();
            let mut new_paths = paths.as_ref().clone();
            new_paths.custom_stream_response_path = Some(path.to_string_lossy().to_string());
            self.paths.store(Arc::new(new_paths));

            let channel_unavailable = load_and_set_file(&path.join(CHANNEL_UNAVAILABLE));
            let user_connections_exhausted = load_and_set_file(&path.join(USER_CONNECTIONS_EXHAUSTED));
            let provider_connections_exhausted = load_and_set_file(&path.join(PROVIDER_CONNECTIONS_EXHAUSTED));
            let low_priority_preempted = load_and_set_file(&path.join(LOW_PRIORITY_PREEMPTED))
                .or_else(|| provider_connections_exhausted.clone());
            let user_account_expired = load_and_set_file(&path.join(USER_ACCOUNT_EXPIRED));
            let panel_api_provisioning = load_and_set_file(&path.join(PANEL_API_PROVISIONING));
            let hls_session_or_lease_expired = load_and_set_file(&path.join(HLS_SESSION_OR_LEASE_EXPIRED));
            let panel_api_provisioning_hls_segments = (0..PANEL_API_PROVISIONING_HLS_SEGMENT_COUNT)
                .filter_map(|index| {
                    let filename = format!("{PANEL_API_PROVISIONING_HLS_SEGMENT_PREFIX}{index:03}.ts");
                    load_and_set_file(&path.join(filename))
                })
                .collect();
            self.custom_stream_response.store(Some(Arc::new(CustomStreamResponse {
                channel_unavailable,
                user_connections_exhausted,
                provider_connections_exhausted,
                low_priority_preempted,
                user_account_expired,
                panel_api_provisioning,
                hls_session_or_lease_expired,
                panel_api_provisioning_hls_segments,
            })));
        }
    }

    pub fn get_server_info(&self, server_info_name: &str) -> Option<ApiProxyServerInfo> {
        let guard = self.api_proxy.load();
        if let Some(api_proxy) = guard.as_ref() {
            let server_info_list = &api_proxy.server;
            server_info_list.iter().find(|c| c.name.eq(server_info_name)).cloned()
        } else {
            None
        }
    }

    pub fn get_user_server_info(&self, user: &ProxyUserCredentials) -> Option<ApiProxyServerInfo> {
        let server_info_name = user.server.as_ref().map_or("default", |server_name| server_name.as_str());
        self.get_server_info(server_info_name)
    }

    pub fn get_disabled_headers(&self) -> Option<ReverseProxyDisabledHeaderConfig> {
        let config = self.config.load();
        config.get_disabled_headers()
    }

    pub fn get_grace_options(&self) -> GracePeriodOptions { self.config.load().get_grace_options() }

    pub fn get_geoip_unavailable_policy(&self) -> GeoIpUnavailablePolicy {
        self.config.load().get_geoip_unavailable_policy()
    }

    pub async fn is_ffprobe_enabled(&self) -> bool {
        let ffprobe_enabled_in_config = {
            let config = self.config.load();
            config.metadata_update.as_ref().is_some_and(|metadata| metadata.ffprobe.enabled)
        };
        if !ffprobe_enabled_in_config {
            return false;
        }

        self.media_tools.is_ffprobe_available().await
    }

    pub async fn is_ffmpeg_available(&self) -> bool { self.media_tools.is_ffmpeg_available().await }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        model::{CompiledMapping, CompiledMappingRule, ConfigSource, MappingProgram},
        utils::FileLockManager,
    };
    use shared::{
        foundation::{Filter, MapperScript},
        model::{ConfigPaths, ProcessingOrder},
    };

    fn empty_paths() -> ConfigPaths {
        ConfigPaths {
            home_path: String::new(),
            config_path: String::new(),
            storage_path: String::new(),
            config_file_path: String::new(),
            sources_file_path: String::new(),
            mapping_file_path: None,
            mapping_files_used: None,
            template_file_path: None,
            template_files_used: None,
            api_proxy_file_path: String::new(),
            custom_stream_response_path: None,
        }
    }

    fn test_app_config_with_target(target: Arc<ConfigTarget>) -> AppConfig {
        AppConfig {
            config: Arc::new(ArcSwap::from_pointee(Config::default())),
            sources: Arc::new(ArcSwap::from_pointee(SourcesConfig {
                sources: vec![ConfigSource { inputs: Vec::new(), targets: vec![target] }],
                ..SourcesConfig::default()
            })),
            hdhomerun: Arc::new(ArcSwapOption::default()),
            api_proxy: Arc::new(ArcSwapOption::default()),
            file_locks: Arc::new(FileLockManager::default()),
            paths: Arc::new(ArcSwap::from_pointee(empty_paths())),
            custom_stream_response: Arc::new(ArcSwapOption::default()),
            access_token_secret: [0; 32],
            encrypt_secret: [0; 16],
            media_tools: Arc::new(MediaToolCapabilities::new()),
        }
    }

    fn target_with_mapping_id(mapping_id: &str) -> Arc<ConfigTarget> {
        Arc::new(ConfigTarget {
            id: 1,
            enabled: true,
            name: "target".to_string(),
            options: None,
            sort: None,
            filter: Filter::default(),
            output: Vec::new(),
            rename: None,
            mapping_ids: Some(vec![mapping_id.to_string()]),
            mapping: Arc::new(ArcSwapOption::default()),
            favourites: None,
            processing_order: ProcessingOrder::Frm,
            execution_plan: crate::model::TargetExecutionPlan::default(),
            watch: None,
            use_memory_cache: false,
        })
    }

    fn mappings_with_one_mapper(mapping_id: &str) -> CompiledMappings {
        CompiledMappings::new(vec![CompiledMapping {
            id: mapping_id.to_string(),
            rules: vec![CompiledMappingRule {
                name: None,
                filter: Filter::default(),
                program: MappingProgram::Script(MapperScript::parse("", None).expect("empty script should parse")),
            }],
            ..CompiledMapping::default()
        }])
    }

    fn empty_mappings() -> CompiledMappings { CompiledMappings::default() }

    #[test]
    fn set_mappings_clears_existing_target_mappings_when_reload_is_empty() {
        let target = target_with_mapping_id("map1");
        let app_config = test_app_config_with_target(Arc::clone(&target));

        app_config.set_mappings("mappings", &mappings_with_one_mapper("map1"));
        assert!(target.mapping.load().is_some(), "initial mapping registration should attach mapping to target");

        app_config.set_mappings("mappings", &empty_mappings());
        assert!(target.mapping.load().is_none(), "empty mapping reload must clear stale target mappings");
    }

    #[test]
    fn set_mappings_does_not_attach_an_unrelated_mapping_for_unknown_id() {
        let target = target_with_mapping_id("missing");
        let app_config = test_app_config_with_target(Arc::clone(&target));

        app_config.set_mappings("mappings", &mappings_with_one_mapper("available"));

        assert!(target.mapping.load().is_none(), "an unknown mapping id must not attach another mapping");
    }

    #[test]
    fn set_mapping_path_stores_resolved_path_in_paths() {
        let app_config = test_app_config_with_target(target_with_mapping_id("map1"));
        app_config.set_mapping_path(Some("custom/mappings.yaml"));
        let stored = app_config.paths.load().mapping_file_path.clone();
        assert!(stored.is_some(), "mapping file path should be stored");
        assert!(
            stored.as_deref().unwrap_or_default().contains("custom/mappings.yaml"),
            "stored path should contain requested relative mapping path, got {stored:?}"
        );
    }

    #[test]
    fn set_template_path_stores_resolved_path_in_paths() {
        let app_config = test_app_config_with_target(target_with_mapping_id("map1"));
        app_config.set_template_path(Some("custom/templates.yaml"));
        let stored = app_config.paths.load().template_file_path.clone();
        assert!(stored.is_some(), "template file path should be stored");
        assert!(
            stored.as_deref().unwrap_or_default().contains("custom/templates.yaml"),
            "stored path should contain requested relative template path, got {stored:?}"
        );
    }

    #[test]
    fn set_mapping_path_unchanged_value_does_not_reallocate_paths() {
        let app_config = test_app_config_with_target(target_with_mapping_id("map1"));
        app_config.set_mapping_path(Some("mappings.yaml"));
        let pointer_before = Arc::as_ptr(&app_config.paths.load()).cast::<u8>();
        // Calling with the same value must not allocate a new ConfigPaths.
        app_config.set_mapping_path(Some("mappings.yaml"));
        let pointer_after = Arc::as_ptr(&app_config.paths.load()).cast::<u8>();
        assert_eq!(pointer_before, pointer_after, "unchanged path must not reallocate");
    }
}
