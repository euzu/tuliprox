use crate::{
    error::TuliproxError,
    foundation::prepare_templates,
    info_err_res,
    model::{
        config::target::ConfigTargetDto, ConfigInputDto, ConfigProviderDto, HdHomeRunDeviceOverview, PatternTemplate,
    },
    utils::{arc_str_vec_serde, default_as_default, Internable},
};
use std::{collections::HashSet, sync::Arc};

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
            return info_err_res!("At least one input should be defined at source: {index}");
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
    ) -> Result<(), TuliproxError> {
        let prepared_templates = self.prepare_templates()?;
        let provider_names = self.prepare_providers()?;
        self.prepare_sources(include_computed, hdhr_config, &provider_names, prepared_templates.as_ref())?;
        self.check_unique_target_names()?;
        Ok(())
    }

    fn prepare_providers(&mut self) -> Result<HashSet<String>, TuliproxError> {
        let mut names = HashSet::new();
        if let Some(providers) = &mut self.provider {
            for provider in providers {
                provider.prepare()?;
                if names.contains(provider.name.as_ref()) {
                    return info_err_res!("Provider names should be unique: {}", provider.name);
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
        prepared_templates: Option<&Vec<PatternTemplate>>,
    ) -> Result<(), TuliproxError> {
        // prepare sources and set id's
        let mut source_index: u16 = 0;
        let mut input_index: u16 = 0;
        let mut target_index: u16 = 1;
        // Prepare global inputs
        for input in &mut self.inputs {
            input_index = input.prepare(input_index, include_computed, provider_names)?;
        }

        for source in &mut self.sources {
            source_index = source.prepare(source_index, include_computed)?;

            // Validate referenced inputs
            for name in &source.inputs {
                if !self.inputs.iter().any(|i| &i.name == name) {
                    return info_err_res!("Source references unknown input: '{name}'");
                }
            }

            for target in &mut source.targets {
                target.prepare(target_index, prepared_templates, hdhr_config)?;
                target_index += 1;
            }
        }
        Ok(())
    }

    fn prepare_templates(&self) -> Result<Option<Vec<PatternTemplate>>, TuliproxError> {
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
        let default_target_name = default_as_default();
        for source in &self.sources {
            for target in &source.targets {
                // check the target name is unique
                let target_name = target.name.as_str();
                if !default_target_name.eq_ignore_ascii_case(target_name) {
                    if seen_names.contains(target_name) {
                        return info_err_res!("target names should be unique: {target_name}");
                    }
                    seen_names.insert(target_name);
                }
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
        model::{ConfigRenameDto, ConfigTargetDto, InputType, ItemField, M3uTargetOutputDto, TargetOutputDto},
        utils::Internable,
    };

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
                    filter: "!FILTER_NAME!".to_string(),
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

        sources.prepare(false, None).expect("sources prepare should succeed");

        assert_eq!(sources.templates, original_templates);
        assert_eq!(sources.sources[0].targets[0].filter, original_filter);
        assert_eq!(
            sources.sources[0].targets[0].rename.as_ref().expect("rename should exist after prepare")[0].pattern,
            original_rename_pattern
        );
    }
}
