use std::collections::HashSet;
use crate::create_tuliprox_error_result;
use crate::error::{handle_tuliprox_error_result_list, TuliproxError, TuliproxErrorKind};
use crate::foundation::filter::prepare_templates;
use crate::model::{ConfigInputDto, PatternTemplate};
use crate::model::config::target::ConfigTargetDto;
use crate::utils::default_as_default;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigSourceDto {
    pub inputs: Vec<ConfigInputDto>,
    pub targets: Vec<ConfigTargetDto>,
}

impl ConfigSourceDto {
    #[allow(clippy::cast_possible_truncation)]
    pub fn prepare(&mut self, index: u16, include_computed: bool) -> Result<u16, TuliproxError> {
        handle_tuliprox_error_result_list!(TuliproxErrorKind::Info, self.inputs.iter_mut().enumerate()
            .map(|(idx, i)| i.prepare(index+(idx as u16), include_computed)));
        Ok(index + (self.inputs.len() as u16))
    }
}

#[derive(Default, Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourcesConfigDto {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub templates: Option<Vec<PatternTemplate>>,
    pub sources: Vec<ConfigSourceDto>,
}

impl SourcesConfigDto {
    pub fn prepare(&mut self, include_computed: bool) -> Result<(), TuliproxError> {
        self.prepare_templates()?;
        self.prepare_sources(include_computed)?;
        self.check_unique_target_names()?;
        Ok(())
    }

    fn prepare_sources(&mut self, include_computed: bool) -> Result<(), TuliproxError> {
        // prepare sources and set id's
        let mut source_index: u16 = 1;
        let mut target_index: u16 = 1;
        for source in &mut self.sources {
            source_index = source.prepare(source_index, include_computed)?;
            for target in &mut source.targets {
                // prepare target templates
                let prepare_result = match &self.templates {
                    Some(templ) => target.prepare(target_index, Some(templ)),
                    _ => target.prepare(target_index, None)
                };
                prepare_result?;
                target_index += 1;
            }
        }
        Ok(())
    }

    fn prepare_templates(&mut self) -> Result<(), TuliproxError> {
        if let Some(templates) = &mut self.templates {
            match prepare_templates(templates) {
                Ok(tmplts) => {
                    self.templates = Some(tmplts);
                }
                Err(err) => {
                    return Err(err);
                }
            }
        }
        Ok(())
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
                        return create_tuliprox_error_result!(TuliproxErrorKind::Info, "target names should be unique: {target_name}");
                    }
                    seen_names.insert(target_name);
                }
            }
        }
        Ok(())
    }
}