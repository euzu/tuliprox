use crate::model::{macros, ConfigInput, ConfigTarget, ProcessTargets};
use shared::error::{info_err_res, TuliproxError};
use shared::model::{ConfigProviderDto, ConfigSourceDto, PatternTemplate, SourcesConfigDto};
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

#[derive(Debug)]
pub struct ConfigProvider {
    pub name: Arc<str>,
    pub urls: Vec<Arc<str>>,
    pub current_url_index: AtomicUsize,
}

impl Clone for ConfigProvider {
    fn clone(&self) -> Self {
        Self {
            name: self.name.clone(),
            urls: self.urls.clone(),
            current_url_index: AtomicUsize::new(self.current_url_index.load(Ordering::Relaxed)),
        }
    }
}


macros::from_impl!(ConfigProvider);
impl From<&ConfigProviderDto> for ConfigProvider {
    fn from(dto: &ConfigProviderDto) -> Self {
        Self {
            name: dto.name.clone(),
            urls: dto.urls.clone(),
            current_url_index: AtomicUsize::new(0)
        }
    }
}

impl ConfigProvider {
    /// Gets the current URL from the provider
    pub fn get_current_url(&self) -> Option<&Arc<str>> {
        let index = self.current_url_index.load(Ordering::Relaxed);
        self.urls.get(index)
    }

    /// Rotates to the next URL in the provider and returns it
    pub fn rotate_to_next_url(&self) -> Option<&Arc<str>> {
        if self.urls.is_empty() {
            return None;
        }

        let current = self.current_url_index.load(Ordering::Relaxed);
        let next = (current + 1) % self.urls.len();
        self.current_url_index.store(next, Ordering::Relaxed);

        self.urls.get(next)
    }

    /// Resets the current URL index to 0
    pub fn reset_index(&self) {
        self.current_url_index.store(0, Ordering::Relaxed);
    }

}

#[derive(Debug, Clone)]
pub struct ConfigSource {
    pub inputs: Vec<Arc<str>>,
    pub targets: Vec<Arc<ConfigTarget>>,
}

impl ConfigSource {
    // Determines whether this source should be processed for the given user targets.
    //
    // Returns `true` if:
    // - `user_targets.targets` is empty (process all sources), OR
    // - At least one target in this source matches an ID in `user_targets.targets`
    //
    // Returns `false` otherwise.
    pub fn should_process_for_user_targets(&self, user_targets: &ProcessTargets) -> bool {
        user_targets.targets.is_empty()
            || self.targets.iter().any(|t| user_targets.targets.contains(&t.id))
    }
}

// macros::try_from_impl!(ConfigSource);
impl ConfigSource {
    pub fn from_dto(dto: &ConfigSourceDto) -> Result<ConfigSource, TuliproxError> {
        Ok(Self {
            inputs: dto.inputs.clone(),
            targets: dto.targets.iter().map(|c| Arc::new(ConfigTarget::from(c))).collect(),
        })
    }
}

#[derive(Default, Debug, Clone)]
pub struct SourcesConfig {
    pub batch_files: Vec<PathBuf>,
    pub templates: Option<Vec<PatternTemplate>>,
    pub provider: Vec<Arc<ConfigProvider>>,
    pub inputs: Vec<Arc<ConfigInput>>,
    pub sources: Vec<ConfigSource>,
}

macros::try_from_impl!(SourcesConfig);
impl TryFrom<&SourcesConfigDto> for SourcesConfig {
    type Error = TuliproxError;
    fn try_from(dto: &SourcesConfigDto) -> Result<Self, TuliproxError> {
        let mut inputs = Vec::<Arc<ConfigInput>>::new();
        let mut batch_files = Vec::<PathBuf>::new();
        let mut input_names = HashSet::new();
        let provider = dto.provider.iter().map(ConfigProvider::from).map(Arc::new).collect::<Vec<_>>();


        for input_dto in &dto.inputs {
            let mut input = ConfigInput::from(input_dto);
            // Prepare input
            if let Some(path) = input.prepare(&provider)? {
                batch_files.push(path);
            }
            input_names.insert(input.name.clone());
            inputs.push(Arc::new(input));
        }

        let mut sources = Vec::new();
        for source_dto in &dto.sources {
            // Validate that all input references exist
            for input_name in &source_dto.inputs {
                if !input_names.contains(input_name) {
                    return info_err_res!("Source references unknown input: {input_name}");
                }
            }
            sources.push(ConfigSource::from_dto(source_dto)?);
        }

        Ok(Self {
            batch_files,
            templates: dto.templates.clone(),
            provider,
            inputs,
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

    pub fn get_source_inputs_by_target_by_name(&self, target_name: &str) -> Option<Vec<Arc<str>>> {
        for source in &self.sources {
            for target in &source.targets {
                if target.name == target_name {
                    return Some(source.inputs.clone());
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
        let inputs: Vec<u16> = self.inputs.iter().map(|i| i.id).collect();
        let mut targets: Vec<u16> = vec![];
        let mut target_names: Vec<String> = vec![];
        if let Some(user_targets) = target_args {
            let mut check_targets: HashMap<String, u16> = user_targets.iter().map(|t| (t.to_lowercase(), 0)).collect();
            for source in &self.sources {
                for target in &source.targets {
                    for user_target in user_targets {
                        let key = user_target.to_lowercase();
                        if target.name.eq_ignore_ascii_case(key.as_str()) {
                            targets.push(target.id);
                            target_names.push(target.name.clone());
                            if let Some(value) = check_targets.get(key.as_str()) {
                                check_targets.insert(key, value + 1);
                            }
                        }
                    }
                }
            }

            let missing_targets: Vec<String> = check_targets.iter().filter(|&(_, v)| *v == 0).map(|(k, _)| k.clone()).collect();
            if !missing_targets.is_empty() {
                return info_err_res!("No target found for {}", missing_targets.join(", "));
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

    pub fn get_unique_target_names(&self) -> HashSet<Cow<'_, str>> {
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
        for file in &self.batch_files {
            file_names.insert(file.clone());
        }
        file_names
    }

    pub fn get_input_by_name(&self, name: &Arc<str>) -> Option<&Arc<ConfigInput>> {
        self.inputs.iter().find(|i| &i.name == name)
    }

    pub fn get_provider_by_name(&self, name: &str) -> Option<&Arc<ConfigProvider>> {
        self.provider.iter().find(|p| p.name.as_ref() == name)
    }
}
