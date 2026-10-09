use super::{ConfigInputAliasDto, ConfigInputDto, InputType, StagedInputType};
use crate::{
    check_input_connections, check_input_credentials,
    error::TuliproxError,
    model::{PatternTemplate, Prepare},
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
            | InputType::Staged => {}
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
