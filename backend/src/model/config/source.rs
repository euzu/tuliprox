use crate::model::{macros, ConfigInput, ConfigTarget, ProcessTargets};
use shared::error::{create_tuliprox_error_result, TuliproxError, TuliproxErrorKind};
use shared::model::{ConfigSourceDto, PatternTemplate, SourcesConfigDto};
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct ConfigSource {
    pub batch_files: Vec<PathBuf>,
    pub inputs: Vec<Arc<ConfigInput>>,
    pub targets: Vec<Arc<ConfigTarget>>,
}

impl ConfigSource {

    pub fn get_inputs_for_target(&self, target_name: &str) -> Option<Vec<Arc<ConfigInput>>> {
        for target in &self.targets {
            if target.name.eq(target_name) {
                let inputs = self.inputs.iter().filter(|&i| i.enabled).map(Arc::clone).collect::<Vec<Arc<ConfigInput>>>();
                if !inputs.is_empty() {
                    return Some(inputs);
                }
            }
        }
        None
    }
}

macros::try_from_impl!(ConfigSource);
impl TryFrom<&ConfigSourceDto> for ConfigSource {
    type Error = TuliproxError;
    fn try_from(dto: &ConfigSourceDto) -> Result<ConfigSource, TuliproxError> {
        let mut batch_files = Vec::new();
        let mut inputs: Vec<ConfigInput> = dto.inputs.iter().map(ConfigInput::from).collect();
        for input in &mut inputs {
            if let Some(batch_file_path) = input.prepare()? {
                batch_files.push(batch_file_path);
            }
        }
        Ok(Self {
            batch_files,
            inputs: inputs.into_iter().map(Arc::new).collect(),
            targets: dto.targets.iter().map(|c| Arc::new(ConfigTarget::from(c))).collect(),
        })
    }
}

#[derive(Default, Debug, Clone)]
pub struct SourcesConfig {
    pub templates: Option<Vec<PatternTemplate>>,
    pub sources: Vec<ConfigSource>,
}

macros::try_from_impl!(SourcesConfig);
impl TryFrom<&SourcesConfigDto> for SourcesConfig {
    type Error = TuliproxError;
    fn try_from(dto: &SourcesConfigDto) -> Result<Self, TuliproxError> {
        let sources = dto.sources
            .iter()
            .map(ConfigSource::try_from)
            .collect::<Result<Vec<_>, _>>()?;

        Ok(Self {
            templates: dto.templates.clone(),
            sources,
        })
    }
}

impl SourcesConfig {
    pub(crate) fn get_source_at(&self, idx: usize) -> Option<&ConfigSource> {
        self.sources.get(idx)
    }

    pub fn get_target_by_id(&self, target_id: u16) -> Option<Arc<ConfigTarget>> {
        for source in &self.sources {
            for target in &source.targets {
                if target.id == target_id {
                    return Some(Arc::clone(target));
                }
            }
        }
        None
    }


    /// Returns the targets that were specified as parameters.
    /// If invalid targets are found, the program will be terminated.
    /// The return value has `enabled` set to true, if selective targets should be processed, otherwise false.
    ///
    /// * `target_args` the program parameters given with `-target` parameter.
    /// * `sources` configured sources in config file
    ///
    pub fn validate_targets(&self, target_args: Option<&Vec<String>>) -> Result<ProcessTargets, TuliproxError> {
        let mut enabled = true;
        let mut inputs: Vec<u16> = vec![];
        let mut targets: Vec<u16> = vec![];
        let mut target_names: Vec<String> = vec![];
        if let Some(user_targets) = target_args {
            let mut check_targets: HashMap<String, u16> = user_targets.iter().map(|t| (t.to_lowercase(), 0)).collect();
            for source in &self.sources {
                let mut target_added = false;
                for target in &source.targets {
                    for user_target in user_targets {
                        let key = user_target.to_lowercase();
                        if target.name.eq_ignore_ascii_case(key.as_str()) {
                            targets.push(target.id);
                            target_names.push(target.name.to_string());
                            target_added = true;
                            if let Some(value) = check_targets.get(key.as_str()) {
                                check_targets.insert(key, value + 1);
                            }
                        }
                    }
                }
                if target_added {
                    source.inputs.iter().map(|i| i.id).for_each(|id| inputs.push(id));
                }
            }

            let missing_targets: Vec<String> = check_targets.iter().filter(|&(_, v)| *v == 0).map(|(k, _)| k.to_string()).collect();
            if !missing_targets.is_empty() {
                return create_tuliprox_error_result!(TuliproxErrorKind::Info, "No target found for {}", missing_targets.join(", "));
            }
            // let processing_targets: Vec<String> = check_targets.iter().filter(|&(_, v)| *v != 0).map(|(k, _)| k.to_string()).collect();
            // info!("Processing targets {}", processing_targets.join(", "));
        } else {
            enabled = false;
        }

        Ok(ProcessTargets {
            enabled,
            inputs,
            targets,
            target_names,
        })
    }

    pub fn get_unique_target_names(&self) -> HashSet<Cow<str>> {
        let mut seen_names = HashSet::new();
        for source in &self.sources {
            for target in &source.targets {
                // check the target name is unique
                let target_name = Cow::Borrowed(target.name.as_str());
                seen_names.insert(target_name);
            }
        }
        seen_names
    }

    pub fn get_input_files(&self) -> HashSet<PathBuf> {
        let mut file_names = HashSet::new();
        for source in &self.sources {
            for file in &source.batch_files {
                file_names.insert(file.clone());
            }
        }
        file_names
    }
}
