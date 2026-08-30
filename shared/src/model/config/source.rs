use crate::{
    error::TuliproxError,
    foundation::prepare_templates,
    model::{
        config::target::ConfigTargetDto, ConfigInputDto, ConfigProviderDto, HdHomeRunDeviceOverview, PatternTemplate,
    },
    utils::{arc_str_vec_serde, is_sanitize_sensitive_info_enabled, Internable},
};
use log::warn;
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

const MAX_STAGE_CHAIN_DEPTH: usize = 2;

#[derive(Clone, Copy)]
struct CredentialOwner<'a> {
    kind: &'static str,
    name: &'a str,
    url: &'a str,
}

fn sensitive_url_for_log(url: &str, sanitize: bool) -> &str {
    if sanitize {
        "***"
    } else {
        url
    }
}

fn duplicate_credentials_warning(
    current: CredentialOwner<'_>,
    previous: CredentialOwner<'_>,
    sanitize: bool,
) -> String {
    let current_url = sensitive_url_for_log(current.url, sanitize);
    if current.url == previous.url {
        format!(
            "The {} '{}' uses the same URL and credentials as the {} '{}' (URL: '{current_url}', username: '***', password: '***'). Tuliprox tracks provider connection limits separately for each input or alias, so the provider's actual connection limit may be exceeded. Reuse the existing provider account definition across multiple targets instead of defining it twice.",
            current.kind, current.name, previous.kind, previous.name
        )
    } else {
        let previous_url = sensitive_url_for_log(previous.url, sanitize);
        format!(
            "The {} '{}' uses the same credentials as the {} '{}', but their URLs differ (URLs: '{current_url}' and '{previous_url}', username: '***', password: '***'). Tuliprox cannot determine whether both URLs point to the same provider account. If they do, connection limits are tracked separately for each input or alias, so the provider's actual connection limit may be exceeded.",
            current.kind, current.name, previous.kind, previous.name
        )
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ConfigSourceDto {
    #[serde(with = "arc_str_vec_serde")]
    pub inputs: Vec<Arc<str>>,
    pub targets: Vec<ConfigTargetDto>,
}

impl ConfigSourceDto {
    #[allow(clippy::cast_possible_truncation)]
    pub fn prepare(&mut self, index: u16, _include_computed: bool) -> Result<u16, TuliproxError> {
        let current_index = index;
        if self.inputs.is_empty() {
            return Err(TuliproxError::ConfigSource(format!(
                "At least one input should be defined at source: {index}"
            )));
        }
        // Trim all input names
        for input in &mut self.inputs {
            *input = input.trim().intern();
        }
        Ok(current_index)
    }
}

#[derive(Default, Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SourcesConfigDto {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub templates: Option<Vec<PatternTemplate>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<Vec<ConfigProviderDto>>,
    pub inputs: Vec<ConfigInputDto>,
    pub sources: Vec<ConfigSourceDto>,
}

impl SourcesConfigDto {
    pub fn prepare(
        &mut self,
        include_computed: bool,
        hdhr_config: Option<&HdHomeRunDeviceOverview>,
        prepared_templates: Option<&[PatternTemplate]>,
    ) -> Result<(), TuliproxError> {
        let local_prepared_templates =
            if prepared_templates.is_none() { self.prepare_local_templates()? } else { None };
        let templates_to_use = prepared_templates.or(local_prepared_templates.as_deref());
        let provider_names = self.prepare_providers()?;
        self.prepare_sources(include_computed, hdhr_config, &provider_names, templates_to_use)?;
        self.check_unique_target_names()?;
        Ok(())
    }

    fn prepare_providers(&mut self) -> Result<HashSet<String>, TuliproxError> {
        let mut names = HashSet::new();
        if let Some(providers) = &mut self.provider {
            for provider in providers {
                provider.prepare()?;
                if names.contains(provider.name.as_ref()) {
                    return Err(TuliproxError::ConfigSource(format!(
                        "Provider names should be unique: {}",
                        provider.name
                    )));
                }
                names.insert(provider.name.to_string());
            }
        }
        Ok(names)
    }

    fn prepare_sources(
        &mut self,
        include_computed: bool,
        hdhr_config: Option<&HdHomeRunDeviceOverview>,
        provider_names: &HashSet<String>,
        prepared_templates: Option<&[PatternTemplate]>,
    ) -> Result<(), TuliproxError> {
        // prepare sources and set id's
        let mut source_index: u16 = 0;
        let mut input_index: u16 = 0;
        let mut target_index: u16 = 1;
        let mut input_credentials = HashMap::new();
        // Prepare global inputs
        for input in &mut self.inputs {
            input_index = input.prepare(input_index, include_computed, provider_names, prepared_templates)?;
            if let (Some(username), Some(password)) = (input.username.as_ref(), input.password.as_ref()) {
                let key = (username.as_str(), password.as_str());
                let current = CredentialOwner { kind: "input", name: input.name.as_ref(), url: input.url.as_str() };
                if let Some(previous) = input_credentials.get(&key) {
                    warn!(
                        "{}",
                        duplicate_credentials_warning(current, *previous, is_sanitize_sensitive_info_enabled())
                    );
                } else {
                    input_credentials.insert(key, current);
                }
            }

            if let Some(aliases) = &input.aliases {
                for alias in aliases {
                    if let (Some(username), Some(password)) = (alias.username.as_ref(), alias.password.as_ref()) {
                        let key = (username.as_str(), password.as_str());
                        let current =
                            CredentialOwner { kind: "input alias", name: alias.name.as_ref(), url: alias.url.as_str() };
                        if let Some(previous) = input_credentials.get(&key) {
                            warn!(
                                "{}",
                                duplicate_credentials_warning(current, *previous, is_sanitize_sensitive_info_enabled())
                            );
                        } else {
                            input_credentials.insert(key, current);
                        }
                    }
                }
            }
        }

        self.validate_staged_providers()?;

        for source in &mut self.sources {
            source_index = source.prepare(source_index, include_computed)?;

            // Validate referenced inputs
            for name in &source.inputs {
                let Some(input) = self.inputs.iter().find(|i| &i.name == name) else {
                    return Err(TuliproxError::ConfigSource(format!("Source references unknown input: '{name}'")));
                };
                if input.input_type.is_staged() {
                    return Err(TuliproxError::ConfigSource(format!(
                        "Source references staged input '{name}' directly; connect staged inputs to an m3u or xtream provider input instead"
                    )));
                }
            }

            for target in &mut source.targets {
                target.prepare(target_index, prepared_templates, hdhr_config)?;
                target_index += 1;
            }
        }
        Ok(())
    }

    fn validate_staged_providers(&self) -> Result<(), TuliproxError> {
        if !self.inputs.iter().any(|input| input.input_type.is_staged()) {
            return Ok(());
        }

        let by_name: std::collections::HashMap<&str, &ConfigInputDto> =
            self.inputs.iter().map(|input| (input.name.as_ref(), input)).collect();
        let mut staged_by_provider = std::collections::HashMap::<&str, &str>::new();

        for input in &self.inputs {
            if !input.input_type.is_staged() {
                continue;
            }
            let Some(staged) = input.staged.as_ref() else {
                continue; // already enforced in ConfigInputDto::prepare
            };
            let Some(provider_name) = staged.for_input.as_deref() else {
                continue;
            };
            if let Some(existing_staged) = staged_by_provider.insert(provider_name, input.name.as_ref()) {
                return Err(TuliproxError::ConfigSource(format!(
                    "provider input '{provider_name}' is referenced by multiple staged inputs: '{existing_staged}' and '{}'",
                    input.name
                )));
            }
            let Some(provider) = by_name.get(provider_name) else {
                return Err(TuliproxError::ConfigSource(format!(
                    "staged input '{}' references unknown provider input '{provider_name}'",
                    input.name
                )));
            };
            if provider.input_type.is_staged() {
                return Err(TuliproxError::ConfigSource(format!(
                    "staged input '{}' cannot use another staged input '{provider_name}' as provider (max chain depth is {MAX_STAGE_CHAIN_DEPTH})",
                    input.name
                )));
            }
            if !(provider.input_type.is_m3u() || provider.input_type.is_xtream()) {
                return Err(TuliproxError::ConfigSource(format!(
                    "staged input '{}' provider '{provider_name}' must be an m3u or xtream input (found: {})",
                    input.name, provider.input_type
                )));
            }
        }
        Ok(())
    }

    fn prepare_local_templates(&self) -> Result<Option<Vec<PatternTemplate>>, TuliproxError> {
        self.templates
            .as_ref()
            .map(|templates| {
                let mut cloned_templates = templates.clone();
                prepare_templates(&mut cloned_templates)
            })
            .transpose()
    }

    fn check_unique_target_names(&self) -> Result<(), TuliproxError> {
        let mut seen_names = HashSet::new();
        for source in &self.sources {
            for target in &source.targets {
                // check the target name is unique
                let target_name = target.name.as_str();
                if seen_names.contains(target_name) {
                    return Err(TuliproxError::ConfigSource(format!("target names should be unique: {target_name}")));
                }
                seen_names.insert(target_name);
            }
        }
        Ok(())
    }

    pub fn get_input(&self, name: &Arc<str>) -> Option<&ConfigInputDto> { self.inputs.iter().find(|i| &i.name == name) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        model::{
            ConfigInputStagedDto, ConfigRenameDto, ConfigTargetDto, InputType, ItemField, M3uTargetOutputDto,
            TargetOutputDto,
        },
        utils::Internable,
    };

    fn staged_overlay(provider: &str) -> ConfigInputDto {
        ConfigInputDto {
            name: "staged".intern(),
            input_type: InputType::Staged,
            url: "http://staged.example/playlist.m3u".to_string(),
            staged: Some(ConfigInputStagedDto { for_input: Some(provider.intern()), ..Default::default() }),
            ..Default::default()
        }
    }

    #[test]
    fn duplicate_credentials_warning_identifies_same_url_account() {
        let previous = CredentialOwner { kind: "input", name: "primary", url: "provider://example" };
        let current = CredentialOwner { kind: "input", name: "duplicate", url: "provider://example" };

        let warning = duplicate_credentials_warning(current, previous, false);

        assert!(warning.contains("same URL and credentials"), "Warning: {warning}");
        assert!(warning.contains("'duplicate'"), "Warning: {warning}");
        assert!(warning.contains("'primary'"), "Warning: {warning}");
        assert!(warning.contains("tracks provider connection limits separately"), "Warning: {warning}");
        assert!(warning.contains("provider://example"), "Warning: {warning}");
        assert!(warning.contains("username: '***', password: '***'"), "Warning: {warning}");
    }

    #[test]
    fn duplicate_credentials_warning_explains_ambiguous_different_urls() {
        let previous = CredentialOwner { kind: "input", name: "primary", url: "https://one.example" };
        let current = CredentialOwner { kind: "input alias", name: "possible-duplicate", url: "https://two.example" };

        let warning = duplicate_credentials_warning(current, previous, false);

        assert!(warning.contains("same credentials"), "Warning: {warning}");
        assert!(warning.contains("URLs differ"), "Warning: {warning}");
        assert!(warning.contains("cannot determine whether both URLs point to the same provider account"));
        assert!(warning.contains("connection limits are tracked separately"), "Warning: {warning}");
        assert!(warning.contains("https://one.example"), "Warning: {warning}");
        assert!(warning.contains("https://two.example"), "Warning: {warning}");
        assert!(warning.contains("username: '***', password: '***'"), "Warning: {warning}");
    }

    #[test]
    fn duplicate_credentials_warning_masks_urls_when_sanitizing_logs() {
        let previous = CredentialOwner { kind: "input", name: "primary", url: "provider://one" };
        let current = CredentialOwner { kind: "input", name: "possible-duplicate", url: "provider://two" };

        let warning = duplicate_credentials_warning(current, previous, true);

        assert!(warning.contains("URLs: '***' and '***'"), "Warning: {warning}");
        assert!(warning.contains("username: '***', password: '***'"), "Warning: {warning}");
        assert!(!warning.contains("provider://"), "Warning must not expose either URL: {warning}");
    }

    #[test]
    fn duplicate_default_target_names_are_rejected() {
        let sources = SourcesConfigDto {
            sources: vec![
                ConfigSourceDto {
                    inputs: vec!["input-a".intern()],
                    targets: vec![ConfigTargetDto { name: "default".to_string(), ..Default::default() }],
                },
                ConfigSourceDto {
                    inputs: vec!["input-b".intern()],
                    targets: vec![ConfigTargetDto { name: "default".to_string(), ..Default::default() }],
                },
            ],
            ..Default::default()
        };

        assert!(sources.check_unique_target_names().is_err());
    }

    #[test]
    fn input_and_target_may_share_a_name() {
        let sources = SourcesConfigDto {
            inputs: vec![ConfigInputDto { name: "shared-name".intern(), ..Default::default() }],
            sources: vec![ConfigSourceDto {
                inputs: vec!["shared-name".intern()],
                targets: vec![ConfigTargetDto { name: "shared-name".to_string(), ..Default::default() }],
            }],
            ..Default::default()
        };

        assert!(sources.check_unique_target_names().is_ok());
    }

    #[test]
    fn prepare_keeps_template_placeholders_in_sources() {
        let templates = vec![
            PatternTemplate {
                name: "BASE".to_string(),
                value: crate::model::TemplateValue::Single(r#"Group ~ "US""#.to_string()),
                placeholder: String::new(),
            },
            PatternTemplate {
                name: "FILTER_NAME".to_string(),
                value: crate::model::TemplateValue::Single("!BASE! AND Type = live".to_string()),
                placeholder: String::new(),
            },
        ];

        let mut sources = SourcesConfigDto {
            templates: Some(templates.clone()),
            inputs: vec![ConfigInputDto {
                name: "input_1".intern(),
                input_type: InputType::M3u,
                url: "http://example.com/playlist.m3u".to_string(),
                ..Default::default()
            }],
            sources: vec![ConfigSourceDto {
                inputs: vec!["input_1".intern()],
                targets: vec![ConfigTargetDto {
                    name: "target_1".to_string(),
                    filter: "!FILTER_NAME!".into(),
                    output: vec![TargetOutputDto::M3u(M3uTargetOutputDto::default())],
                    rename: Some(vec![ConfigRenameDto {
                        field: ItemField::Name,
                        pattern: "!BASE!".to_string(),
                        new_name: "Renamed".to_string(),
                        t_pattern: None,
                    }]),
                    ..Default::default()
                }],
            }],
            ..Default::default()
        };

        let original_templates = sources.templates.clone();
        let original_filter = sources.sources[0].targets[0].filter.clone();
        let original_rename_pattern =
            sources.sources[0].targets[0].rename.as_ref().expect("rename should exist")[0].pattern.clone();

        sources.prepare(false, None, None).expect("sources prepare should succeed");

        assert_eq!(sources.templates, original_templates);
        assert_eq!(sources.sources[0].targets[0].filter, original_filter);
        assert_eq!(
            sources.sources[0].targets[0].rename.as_ref().expect("rename should exist after prepare")[0].pattern,
            original_rename_pattern
        );
    }

    #[test]
    fn prepare_rejects_multiple_staged_inputs_for_one_provider() {
        let mut sources = SourcesConfigDto {
            inputs: vec![
                ConfigInputDto {
                    name: "provider".intern(),
                    input_type: InputType::M3u,
                    url: "http://example.com/playlist.m3u".to_string(),
                    ..Default::default()
                },
                ConfigInputDto {
                    name: "staged_live".intern(),
                    input_type: InputType::Staged,
                    url: "http://staged.example/live.m3u".to_string(),
                    staged: Some(ConfigInputStagedDto { for_input: Some("provider".intern()), ..Default::default() }),
                    ..Default::default()
                },
                ConfigInputDto {
                    name: "staged_vod".intern(),
                    input_type: InputType::Staged,
                    url: "http://staged.example/vod.m3u".to_string(),
                    staged: Some(ConfigInputStagedDto { for_input: Some("provider".intern()), ..Default::default() }),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };

        let err =
            sources.prepare(false, None, None).expect_err("one provider must not accept multiple staged overlays");
        assert!(err.to_string().contains("referenced by multiple staged inputs"), "Error: {err}");
    }

    #[test]
    fn prepare_rejects_staged_unknown_provider() {
        let mut sources = SourcesConfigDto { inputs: vec![staged_overlay("missing_provider")], ..Default::default() };

        let err = sources.prepare(false, None, None).expect_err("unknown staged provider must be rejected");
        assert!(err.to_string().contains("references unknown provider input 'missing_provider'"), "Error: {err}");
    }

    #[test]
    fn prepare_rejects_staged_provider_that_is_staged() {
        let mut sources = SourcesConfigDto {
            inputs: vec![
                ConfigInputDto {
                    name: "root".intern(),
                    input_type: InputType::M3u,
                    url: "http://root.example/playlist.m3u".to_string(),
                    ..Default::default()
                },
                ConfigInputDto {
                    name: "provider".intern(),
                    input_type: InputType::Staged,
                    url: "http://provider.example/playlist.m3u".to_string(),
                    staged: Some(ConfigInputStagedDto { for_input: Some("root".intern()), ..Default::default() }),
                    ..Default::default()
                },
                staged_overlay("provider"),
            ],
            ..Default::default()
        };

        let err = sources.prepare(false, None, None).expect_err("staged provider must not be staged");
        assert!(err.to_string().contains("cannot use another staged input 'provider' as provider"), "Error: {err}");
    }

    #[test]
    fn prepare_rejects_staged_provider_that_is_not_m3u_or_xtream() {
        let mut sources = SourcesConfigDto {
            inputs: vec![
                ConfigInputDto {
                    name: "provider".intern(),
                    input_type: InputType::Library,
                    url: "/media".to_string(),
                    ..Default::default()
                },
                staged_overlay("provider"),
            ],
            ..Default::default()
        };

        let err = sources.prepare(false, None, None).expect_err("non-provider input must be rejected");
        assert!(err.to_string().contains("provider 'provider' must be an m3u or xtream input"), "Error: {err}");
    }

    #[test]
    fn prepare_rejects_staged_input_referenced_by_source() {
        let mut sources = SourcesConfigDto {
            inputs: vec![
                ConfigInputDto {
                    name: "provider".intern(),
                    input_type: InputType::M3u,
                    url: "http://example.com/playlist.m3u".to_string(),
                    ..Default::default()
                },
                staged_overlay("provider"),
            ],
            sources: vec![ConfigSourceDto {
                inputs: vec!["staged".intern()],
                targets: vec![ConfigTargetDto {
                    filter: r#"name ~ ".*""#.into(),
                    output: vec![TargetOutputDto::M3u(M3uTargetOutputDto::default())],
                    ..Default::default()
                }],
            }],
            ..Default::default()
        };

        let err = sources.prepare(false, None, None).expect_err("sources must not reference staged inputs directly");
        assert!(err.to_string().contains("Source references staged input 'staged'"), "Error: {err}");
    }
}
