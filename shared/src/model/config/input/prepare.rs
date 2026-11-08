use super::{ConfigInputAliasDto, ConfigInputDto, InputType, StagedInputType};
use crate::{
    check_input_connections, check_input_credentials,
    error::TuliproxError,
    model::{is_valid_plugin_id, PatternTemplate, Prepare},
    utils::{
        get_credentials_from_url_str, get_trimmed_string, is_non_blank_optional_string, parse_duration_seconds,
        parse_provider_scheme_url_parts, sanitize_sensitive_info, trim_last_slash, Internable, BATCH_SCHEME_PREFIX,
        PROVIDER_SCHEME_PREFIX,
    },
};
use log::warn;
use std::{collections::HashSet, sync::Arc};

impl ConfigInputDto {
    pub(super) fn normalize_input_type_from_batch_url(&mut self) {
        let is_batch_url = self.url.trim().starts_with(BATCH_SCHEME_PREFIX);
        self.input_type = match self.input_type {
            InputType::M3u | InputType::M3uBatch => {
                if is_batch_url {
                    InputType::M3uBatch
                } else {
                    InputType::M3u
                }
            }
            InputType::Xtream | InputType::XtreamBatch => {
                if is_batch_url {
                    InputType::XtreamBatch
                } else {
                    InputType::Xtream
                }
            }
            InputType::Stalker | InputType::StalkerBatch => {
                if is_batch_url {
                    InputType::StalkerBatch
                } else {
                    InputType::Stalker
                }
            }
            InputType::Library => InputType::Library,
            InputType::Emby => InputType::Emby,
            InputType::Jellyfin => InputType::Jellyfin,
            InputType::Plex => InputType::Plex,
            InputType::Staged => InputType::Staged,
            InputType::Plugin => InputType::Plugin,
        };
    }

    pub(super) fn prepare_media_server_input(&mut self) -> Result<(), TuliproxError> {
        if !self.input_type.is_media_server() {
            return Ok(());
        }

        let trimmed_url = self.url.trim();
        if trimmed_url.starts_with(BATCH_SCHEME_PREFIX) || trimmed_url.starts_with(PROVIDER_SCHEME_PREFIX) {
            return Err(TuliproxError::ConfigInput(format!(
                "media-server input does not support batch:// or provider:// URLs (input: {})",
                self.name
            )));
        }
        if self.aliases.as_ref().is_some_and(|aliases| !aliases.is_empty()) {
            return Err(TuliproxError::ConfigInput(format!(
                "media-server input does not support aliases (input: {})",
                self.name
            )));
        }
        if self.epg.is_some() {
            return Err(TuliproxError::ConfigInput(format!(
                "media-server input does not support EPG configuration (input: {})",
                self.name
            )));
        }
        if self.panel_api.is_some() {
            return Err(TuliproxError::ConfigInput(format!(
                "media-server input does not support panel_api configuration (input: {})",
                self.name
            )));
        }
        if self.provider.as_ref().is_some_and(|provider| !provider.is_empty()) {
            return Err(TuliproxError::ConfigInput(format!(
                "media-server input does not support provider failover definitions (input: {})",
                self.name
            )));
        }
        let Some(media_server) = self.media_server.as_mut() else {
            return Err(TuliproxError::ConfigInput(format!(
                "media_server configuration is mandatory for input type {} (input: {})",
                self.input_type, self.name
            )));
        };
        media_server.prepare(&self.name)?;
        if media_server.libraries.is_empty() {
            return Err(TuliproxError::ConfigInput(format!(
                "media-server input requires at least one selected library (input: {})",
                self.name
            )));
        }

        match self.input_type {
            InputType::Emby | InputType::Jellyfin => {
                if trimmed_url.is_empty() {
                    return Err(TuliproxError::ConfigInput(format!(
                        "url is mandatory for input type {} (input: {})",
                        self.input_type, self.name
                    )));
                }
                let has_login = self.username.as_ref().is_some_and(|u| !u.trim().is_empty())
                    && self.password.as_ref().is_some_and(|p| !p.trim().is_empty());
                if !media_server.has_any_emby_jellyfin_auth() && !has_login {
                    return Err(TuliproxError::ConfigInput(format!(
                        "media-server input type {} requires media_server token/api_key or username/password bootstrap credentials (input: {})",
                        self.input_type, self.name
                    )));
                }
            }
            InputType::Plex => {
                if trimmed_url.is_empty() {
                    if !is_non_blank_optional_string(&media_server.account_token) {
                        return Err(TuliproxError::ConfigInput(format!(
                            "media-server input type plex without input.url requires media_server.account_token for MyPlex discovery (input: {})",
                            self.name
                        )));
                    }
                    if !media_server.has_plex_server_selector() {
                        return Err(TuliproxError::ConfigInput(format!(
                            "media-server input type plex requires a server selector such as media_server.server_id or media_server.server_name when input.url is omitted (input: {})",
                            self.name
                        )));
                    }
                } else if !is_non_blank_optional_string(&media_server.token) {
                    return Err(TuliproxError::ConfigInput(format!(
                        "media-server input type plex with input.url requires media_server.token for direct PMS access (input: {})",
                        self.name
                    )));
                }
            }
            InputType::M3u
            | InputType::Xtream
            | InputType::M3uBatch
            | InputType::XtreamBatch
            | InputType::Stalker
            | InputType::StalkerBatch
            | InputType::Library
            | InputType::Staged
            | InputType::Plugin => {}
        }

        Ok(())
    }

    /// `input_type == Plugin <=> plugin.is_some()`, with the id checked against the plugin id
    /// grammar. A plugin input has no provider account, so the account-shaped blocks are rejected
    /// rather than silently ignored. Whether the plugin exists and provides an input type is checked
    /// after plugin load (`ConfigInput::plugin_ref`), since plugins load after the config.
    pub(super) fn validate_plugin_self(&mut self) -> Result<(), TuliproxError> {
        if let Some(raw_id) = self.plugin.as_deref().map(str::trim) {
            if !self.input_type.is_plugin() {
                return Err(TuliproxError::ConfigInput(format!(
                    "plugin is only allowed for input type plugin, not {} (input: {})",
                    self.input_type, self.name
                )));
            }
            if !is_valid_plugin_id(raw_id) {
                return Err(TuliproxError::ConfigInput(format!(
                    "plugin id '{raw_id}' is not valid, expected lowercase letters, digits and '-' (input: {})",
                    self.name
                )));
            }
            self.plugin = Some(raw_id.intern());
        } else if self.input_type.is_plugin() {
            return Err(TuliproxError::ConfigInput(format!(
                "input type plugin requires the `plugin` attribute naming the providing plugin (input: {})",
                self.name
            )));
        }

        if self.input_type.is_plugin() {
            for (present, what) in [
                (self.aliases.as_ref().is_some_and(|aliases| !aliases.is_empty()), "aliases"),
                (self.media_server.is_some(), "media_server"),
                (self.panel_api.is_some(), "panel_api"),
                (self.stalker.is_some(), "stalker"),
                (self.provider.as_ref().is_some_and(|provider| !provider.is_empty()), "provider failover definitions"),
            ] {
                if present {
                    return Err(TuliproxError::ConfigInput(format!(
                        "plugin input does not support {what} (input: {})",
                        self.name
                    )));
                }
            }
            // A dot is the nesting separator of `plugin_options`, so a key holding one would be
            // indistinguishable from a nested property.
            if let Some(bad) = self
                .plugin_config
                .keys()
                .find(|key| key.trim().is_empty() || key.trim() != key.as_str() || key.contains('.'))
            {
                return Err(TuliproxError::ConfigInput(format!(
                    "plugin_config key '{bad}' must be a non-empty name without dots or surrounding spaces (input: {})",
                    self.name
                )));
            }
        } else if !self.plugin_config.is_empty() {
            return Err(TuliproxError::ConfigInput(format!(
                "plugin_config is only allowed for input type plugin, not {} (input: {})",
                self.input_type, self.name
            )));
        }
        Ok(())
    }

    pub(super) fn validate_staged_self(&mut self) -> Result<(), TuliproxError> {
        if self.input_type.is_staged() {
            let Some(staged) = self.staged.as_mut() else {
                return Err(TuliproxError::ConfigInput(format!(
                    "staged input requires a provider input (input: {})",
                    self.name
                )));
            };
            if let Some(for_input) = staged.for_input.as_ref().map(|p| p.trim()).filter(|p| !p.is_empty()) {
                staged.for_input = Some(for_input.intern());
            } else {
                return Err(TuliproxError::ConfigInput(format!(
                    "staged input requires a provider input (input: {})",
                    self.name
                )));
            }
            if staged.clusters.is_empty() {
                return Err(TuliproxError::ConfigInput(format!(
                    "staged input requires at least one staged cluster (input: {})",
                    self.name
                )));
            }
            if self.url.trim().is_empty() {
                return Err(TuliproxError::ConfigInput(format!(
                    "url for staged input is mandatory (input: {})",
                    self.name
                )));
            }
            if self.staged_type == StagedInputType::Xtream {
                let has_credentials = self.username.as_ref().is_some_and(|u| !u.trim().is_empty())
                    && self.password.as_ref().is_some_and(|p| !p.trim().is_empty());
                if !has_credentials {
                    return Err(TuliproxError::ConfigInput(format!(
                        "staged xtream input requires username and password (input: {})",
                        self.name
                    )));
                }
            }
            // R7
            if self.media_server.is_some() {
                return Err(TuliproxError::ConfigInput(format!(
                    "staged input does not support media_server configuration (input: {})",
                    self.name
                )));
            }
            if self.panel_api.is_some() {
                return Err(TuliproxError::ConfigInput(format!(
                    "staged input does not support panel_api configuration (input: {})",
                    self.name
                )));
            }
        } else if self.staged.is_some() {
            return Err(TuliproxError::ConfigInput(format!(
                "staged configuration is only allowed for staged inputs (input: {})",
                self.name
            )));
        }
        Ok(())
    }

    pub fn prepare(
        &mut self,
        index: u16,
        _include_computed: bool,
        provider_names: &HashSet<String>,
        templates: Option<&[PatternTemplate]>,
    ) -> Result<u16, TuliproxError> {
        crate::model::validate_managed_channel_refs(&self.managed_channels).map_err(TuliproxError::ConfigInput)?;
        if !self.managed_channels.is_empty() && (self.input_type == InputType::Staged || self.staged.is_some()) {
            return Err(TuliproxError::ConfigInput(
                "attach managed channels to a provider input or target, not a staged overlay".to_string(),
            ));
        }
        self.name = self.name.trim().intern();
        if self.name.is_empty() {
            return Err(TuliproxError::ConfigInput("name for input is mandatory".to_string()));
        }

        if self.sequential_group == Some(0) {
            return Err(TuliproxError::ConfigInput(format!(
                "sequential_group must be greater than zero (input: {})",
                self.name
            )));
        }
        if self.input_type.is_staged() && self.sequential_group.is_some() {
            return Err(TuliproxError::ConfigInput(format!(
                "staged input does not support sequential_group (input: {})",
                self.name
            )));
        }

        if self.input_type.is_staged() {
            self.priority = 0;
            self.max_connections = 0;
            self.cache_duration = None;
            self.cache_duration_seconds = 0;
        }

        if let Some(duration_str) = &self.cache_duration {
            self.cache_duration_seconds = self.parse_duration(duration_str)?;
        } else {
            self.cache_duration_seconds = 0;
        }

        self.url = self.url.trim().to_string();
        self.normalize_input_type_from_batch_url();
        self.validate_plugin_self()?;
        if let Some(options) = self.options.as_ref() {
            if options.disable_hls_streaming && !self.input_type.is_xtream() {
                return Err(TuliproxError::ConfigInput(format!(
                    "`disable_hls_streaming` is supported only for Xtream inputs (input: {})",
                    self.name
                )));
            }
        }
        if let Some(media_server) = self.media_server.as_mut() {
            media_server.normalize();
        }
        if self.enabled {
            self.prepare_media_server_input()?;
        }
        if self.url.starts_with(PROVIDER_SCHEME_PREFIX) && self.input_type.is_batch() {
            return Err(TuliproxError::ConfigInput(format!(
                "input type {} does not support provider:// URLs for batch definitions; use batch:// URL (input: {})",
                self.input_type, self.name
            )));
        }

        check_input_credentials!(self, self.input_type, true, false);
        check_input_connections!(self, self.input_type, false);
        // Always run stalker validation: it rejects a stray `stalker` block on
        // non-stalker inputs and surfaces a malformed MAC at config load even
        // when the input is currently disabled (consistent with aliases).
        self.prepare_stalker_input()?;
        self.validate_staged_self()?;

        self.persist = get_trimmed_string(self.persist.as_deref());
        check_provider_scheme_url!(self.url, provider_names);
        self.playback_provider = get_trimmed_string(self.playback_provider.as_deref());
        if let Some(name) = self.playback_provider.as_deref() {
            if !provider_names.contains(name) {
                return Err(TuliproxError::ConfigInput(format!(
                    "playback_provider '{name}' is not defined (input: {})",
                    self.name
                )));
            }
        }

        let mut current_index = index + 1;
        self.id = current_index;
        if let Some(aliases) = self.aliases.as_mut() {
            let input_type = &self.input_type;
            for alias in aliases {
                current_index = alias.prepare(current_index, input_type)?;
                check_provider_scheme_url!(alias.url.as_str(), provider_names);
            }
        }

        if let Some(panel_api) = self.panel_api.as_mut() {
            panel_api.prepare(&self.name)?;
        }

        // Validate provider:// URLs in EPG sources
        if let Some(epg) = self.epg.as_ref() {
            if let Some(sources) = epg.sources.as_ref() {
                for epg_source in sources {
                    let url = epg_source.url.trim();
                    check_provider_scheme_url!(url, provider_names);
                }
            }
        }

        // Prepare filter options
        if let Some(options) = self.options.as_mut() {
            options.prepare(templates)?;
        }

        Ok(current_index)
    }

    pub(super) fn parse_duration(&self, duration_str: &str) -> Result<u64, TuliproxError> {
        match parse_duration_seconds(duration_str, false) {
            Some(seconds) => Ok(seconds),
            None => Err(TuliproxError::ConfigInput(format!(
                "Invalid cache_duration format in '{}': {}",
                self.name, duration_str
            ))),
        }
    }

    // Neue ausgelagerte Methode für die URL-Generierung
    pub(super) fn generate_auto_epg_url(&self) -> Result<String, String> {
        let get_creds = || {
            if self.username.is_some() && self.password.is_some() {
                return (self.username.clone(), self.password.clone(), Some(self.url.clone()));
            }

            let (u, p, r) = self
                .aliases
                .as_ref()
                .and_then(|aliases| aliases.iter().find(|a| a.enabled))
                .map_or((None, None, None), |alias| {
                    (alias.username.clone(), alias.password.clone(), Some(alias.url.clone()))
                });

            if u.is_some() && p.is_some() && r.is_some() {
                return (u, p, r);
            }

            let (u, p) = get_credentials_from_url_str(&self.url);
            if u.is_some() && p.is_some() {
                return (u, p, Some(self.url.clone()));
            }

            self.aliases.as_ref().and_then(|aliases| aliases.iter().find(|a| a.enabled)).map_or(
                (None, None, None),
                |alias| {
                    let (u, p) = get_credentials_from_url_str(alias.url.as_str());
                    (u, p, Some(alias.url.clone()))
                },
            )
        };

        let (username, password, base_url) = get_creds();

        if username.is_none() || password.is_none() || base_url.is_none() {
            Err(format!("auto_epg is enabled for input {}, but no credentials could be extracted", self.name))
        } else if let Some(base) = base_url {
            let clean_base = base.split('?').next().unwrap_or(&base);

            let provider_epg_url = format!(
                "{}/xmltv.php?username={}&password={}",
                trim_last_slash(clean_base),
                username.unwrap_or_default(),
                password.unwrap_or_default()
            );
            Ok(provider_epg_url)
        } else {
            Err(format!(
                "auto_epg is enabled for input {}, but url could not be parsed {}",
                self.name,
                sanitize_sensitive_info(&self.url)
            ))
        }
    }

    pub fn prepare_epg(&mut self, include_computed: bool) -> Result<(), TuliproxError> {
        if let Some(mut epg) = self.epg.take() {
            if self.input_type == InputType::Library {
                warn!("EPG is not supported for library inputs {}, skipping", self.name);
                self.epg = None;
                return Ok(());
            }

            epg.prepare(|| self.generate_auto_epg_url(), include_computed)?;
            epg.t_sources = {
                let mut seen_sources = HashSet::new();
                epg.t_sources.drain(..).filter(|src| seen_sources.insert(src.source_identity())).collect()
            };
            self.epg = Some(epg);
        }
        Ok(())
    }

    pub fn prepare_batch(
        &mut self,
        batch_aliases: Vec<ConfigInputAliasDto>,
        index: u16,
    ) -> Result<Option<u16>, TuliproxError> {
        let idx = apply_batch_aliases!(self, batch_aliases, Some(index));
        Ok(idx)
    }

    pub fn prepare_type(&mut self) -> Result<(), TuliproxError> {
        self.url = self.url.trim().to_string();
        self.normalize_input_type_from_batch_url();
        if self.url.starts_with(PROVIDER_SCHEME_PREFIX) && self.input_type.is_batch() {
            return Err(TuliproxError::ConfigInput(format!(
                "input type {} does not support provider:// URLs for batch definitions; use batch:// URL",
                self.input_type
            )));
        }
        Ok(())
    }

    pub fn upsert_alias(&mut self, mut alias: ConfigInputAliasDto) -> Result<(), TuliproxError> {
        check_input_credentials!(alias, self.input_type, true, true);
        check_input_connections!(alias, self.input_type, true);
        let aliases = self.aliases.get_or_insert_with(Vec::new);
        if let Some(existing) = aliases.iter_mut().find(|a| a.id == alias.id) {
            *existing = alias;
        } else {
            aliases.push(alias);
        }
        Ok(())
    }

    pub fn update_account_expiration_date(
        &mut self,
        account_name: &str,
        exp_date: i64,
        disable: bool,
    ) -> Result<bool, TuliproxError> {
        if self.name.as_ref() == account_name {
            let changed = self.exp_date != Some(exp_date) || disable && !self.account_disabled;
            self.exp_date = Some(exp_date);
            self.account_disabled |= disable;
            return Ok(changed);
        }
        let (expiration, enabled) = if let Some(alias) = self
            .aliases
            .as_mut()
            .and_then(|aliases| aliases.iter_mut().find(|alias| alias.name.as_ref() == account_name))
        {
            (&mut alias.exp_date, &mut alias.enabled)
        } else {
            return Err(TuliproxError::ConfigInput(format!("No input or alias found for account '{account_name}'")));
        };
        let changed = *expiration != Some(exp_date) || disable && *enabled;
        *expiration = Some(exp_date);
        if disable {
            *enabled = false;
        }
        Ok(changed)
    }
}

#[cfg(test)]
mod plugin_input_tests {
    use super::*;

    fn prepare(dto: &mut ConfigInputDto) -> Result<u16, TuliproxError> { dto.prepare(0, false, &HashSet::new(), None) }

    fn plugin_input(plugin: Option<&str>) -> ConfigInputDto {
        ConfigInputDto {
            name: "from-plugin".intern(),
            input_type: InputType::Plugin,
            plugin: plugin.map(Internable::intern),
            ..ConfigInputDto::default()
        }
    }

    #[test]
    fn plugin_config_belongs_to_plugin_inputs_and_has_plain_keys() {
        let mut dto = ConfigInputDto {
            plugin_config: serde_json::Map::from_iter([("mode".to_string(), serde_json::json!("live_only"))]),
            ..plugin_input(Some("youtube-input"))
        };
        assert!(prepare(&mut dto).is_ok());

        let mut dotted = ConfigInputDto {
            plugin_config: serde_json::Map::from_iter([("a.b".to_string(), serde_json::json!(1))]),
            ..plugin_input(Some("youtube-input"))
        };
        let err = prepare(&mut dotted).unwrap_err().to_string();
        assert!(err.contains("without dots"), "{err}");

        let mut built_in = ConfigInputDto {
            name: "m3u".intern(),
            url: "http://host/list.m3u".to_string(),
            plugin_config: serde_json::Map::from_iter([("mode".to_string(), serde_json::json!("x"))]),
            ..ConfigInputDto::default()
        };
        let err = prepare(&mut built_in).unwrap_err().to_string();
        assert!(err.contains("only allowed for input type plugin"), "{err}");
    }

    #[test]
    fn plugin_input_requires_the_plugin_id() {
        let err = prepare(&mut plugin_input(None)).unwrap_err().to_string();
        assert!(err.contains("requires the `plugin` attribute"), "{err}");
    }

    #[test]
    fn plugin_id_is_only_allowed_for_plugin_inputs() {
        let mut dto = ConfigInputDto {
            name: "m3u".intern(),
            url: "http://host/list.m3u".to_string(),
            plugin: Some("my-plugin".intern()),
            ..ConfigInputDto::default()
        };
        let err = prepare(&mut dto).unwrap_err().to_string();
        assert!(err.contains("only allowed for input type plugin"), "{err}");
    }

    #[test]
    fn plugin_id_must_be_a_valid_plugin_id() {
        for bad in ["", "  ", "Upper", "x", "has space", "-lead"] {
            let err = prepare(&mut plugin_input(Some(bad))).unwrap_err().to_string();
            assert!(err.contains("is not valid"), "{bad:?}: {err}");
        }
    }

    #[test]
    fn a_plugin_input_needs_neither_url_nor_credentials() {
        let mut dto = plugin_input(Some(" my-plugin "));
        prepare(&mut dto).unwrap();
        assert_eq!(dto.plugin.as_deref(), Some("my-plugin"), "the id is trimmed");
        assert_eq!(dto.input_type, InputType::Plugin, "the batch:// URL normalization leaves it alone");
        assert_eq!(dto.url, "");
        assert!(dto.username.is_none() && dto.password.is_none());
    }

    #[test]
    fn url_and_credentials_are_kept_for_the_plugin() {
        let mut dto = plugin_input(Some("my-plugin"));
        dto.url = " http://host/feed ".to_string();
        dto.username = Some(" user ".to_string());
        dto.password = Some("secret".to_string());
        prepare(&mut dto).unwrap();
        assert_eq!(dto.url, "http://host/feed");
        assert_eq!(dto.username.as_deref(), Some("user"));
        assert_eq!(dto.password.as_deref(), Some("secret"));
    }

    #[test]
    fn account_shaped_blocks_are_rejected_for_a_plugin_input() {
        let mut with_alias = plugin_input(Some("my-plugin"));
        with_alias.aliases = Some(vec![ConfigInputAliasDto::default()]);
        let err = prepare(&mut with_alias).unwrap_err().to_string();
        assert!(err.contains("does not support aliases"), "{err}");

        let mut with_panel = plugin_input(Some("my-plugin"));
        with_panel.panel_api = Some(crate::model::PanelApiConfigDto::default());
        let err = prepare(&mut with_panel).unwrap_err().to_string();
        assert!(err.contains("does not support panel_api"), "{err}");
    }

    #[test]
    fn the_yaml_shape_round_trips() {
        let yaml = "name: from-plugin\ntype: plugin\nplugin: my-plugin\nurl: http://host/feed\n";
        let dto: ConfigInputDto = serde_saphyr::from_str(yaml).unwrap();
        assert_eq!(dto.input_type, InputType::Plugin);
        assert_eq!(dto.plugin.as_deref(), Some("my-plugin"));

        let json = serde_json::to_value(&dto).unwrap();
        assert_eq!(json["type"], "plugin");
        assert_eq!(json["plugin"], "my-plugin");
        let back: ConfigInputDto = serde_json::from_value(json).unwrap();
        assert_eq!(back, dto);

        let without = serde_json::to_value(ConfigInputDto::default()).unwrap();
        assert!(without.get("plugin").is_none(), "an input without a plugin does not serialize the key");
    }

    #[test]
    fn plugin_spells_plugin_in_serde_and_strum() {
        assert_eq!(serde_json::to_value(InputType::Plugin).unwrap(), serde_json::json!("plugin"));
        assert_eq!(serde_json::from_value::<InputType>(serde_json::json!("plugin")).unwrap(), InputType::Plugin);
        assert_eq!(InputType::Plugin.to_string(), "plugin");
        assert_eq!(InputType::Plugin.as_ref(), "plugin");
        assert_eq!("plugin".parse::<InputType>().unwrap(), InputType::Plugin);
        assert_eq!(InputType::default(), InputType::M3u, "the default is unchanged");
    }

    #[test]
    fn plugin_capabilities_are_explicit() {
        let capabilities = InputType::Plugin.capabilities();
        assert_eq!(capabilities.persistence, super::super::InputPersistence::M3u);
        assert!(!capabilities.requires_provider_connection_for_probe);
        assert!(!capabilities.served_on_custom_provider_endpoint);
        assert!(!InputType::Plugin.uses_standard_input_url());
    }
}
