use std::collections::HashMap;
use crate::model::{Mappings};
use shared::error::{create_tuliprox_error_result, info_err, TuliproxError, TuliproxErrorKind};
use crate::utils::traverse_dir;
use crate::utils::{config_file_reader, open_file};
use log::{warn};
use std::path::{Path, PathBuf};
use shared::model::{MappingDefinitionDto, MappingDto, MappingsDto, PatternTemplate};

fn read_mapping(mapping_file: &Path, resolve_var: bool, prepare_mappings: bool) -> Result<Option<MappingsDto>, TuliproxError> {
    if let Ok(file) = open_file(mapping_file) {
        let maybe_mapping: Result<MappingsDto, _> = serde_yaml::from_reader(config_file_reader(file, resolve_var));
        return match maybe_mapping {
            Ok(mut mapping) => {
                if prepare_mappings {
                    mapping.prepare()?;
                }
                Ok(Some(mapping))
            }
            Err(err) => {
                Err(info_err!(err.to_string()))
            }
        };
    }
    warn!("Can't read mapping file: {}", mapping_file.to_str().unwrap_or("?"));
    Ok(None)
}

fn read_mappings_from_file(mappings_file: &Path, resolve_env: bool) -> Result<Option<MappingsDto>, TuliproxError> {
    match read_mapping(mappings_file, resolve_env, true) {
        Ok(mappings) => {
            match mappings {
                None => Ok(None),
                Some(mappings_cfg) => Ok(Some(mappings_cfg))
            }
        }
        Err(err) => Err(err),
    }
}


fn merge_mappings(mappings: Vec<MappingDto>) -> Vec<MappingDto> {
    let mut map: HashMap<String, MappingDto> = HashMap::new();

    for mut m in mappings {
        let entry = map.entry(m.id.clone()).or_insert_with(|| MappingDto {
            id: m.id.clone(),
            ..Default::default()
        });

        // Logic for match_as_ascii: true, if one of them is true
        entry.match_as_ascii |= m.match_as_ascii;

        if let Some(mut mapper) = m.mapper.take() {
            entry.mapper.get_or_insert(vec![]).append(&mut mapper);
        }

        if let Some(mut counters) = m.counter.take() {
            entry.counter.get_or_insert(vec![]).append(&mut counters);
        }
    }

    map.into_values().collect()
}
fn merge_mapping_definitions(mappings: Vec<MappingsDto>) -> Result<Option<MappingsDto>, TuliproxError> {
    let mut merged_templates: Vec<PatternTemplate> = Vec::new();
    let mut merged_mapping: Vec<MappingDto> = Vec::new();

    for mapping in mappings {
        if let Some(mut templates) = mapping.mappings.templates {
            merged_templates.append(&mut templates);
        }

         merged_mapping.extend(mapping.mappings.mapping);
    }

    let mut result = MappingsDto {
        mappings: MappingDefinitionDto {
            templates: if merged_templates.is_empty() { None } else { Some(merged_templates) },
            mapping: merge_mappings(merged_mapping)
        }
    };
    result.prepare()?;
    Ok(Some(result))
}

fn read_mappings_from_directory(path: &Path, resolve_env: bool) -> Result<Option<MappingsDto>, TuliproxError> {
    let mut files = vec![];
    let mut visit = |entry: &std::fs::DirEntry, metadata: &std::fs::Metadata| {
        if metadata.is_file() {
            let file_path = entry.path();
            if file_path.extension().is_some_and(|ext| ext == "yml") {
                files.push(file_path);
            }
        }
    };
    traverse_dir(path, &mut visit).map_err(|err| TuliproxError::new(TuliproxErrorKind::Info, format!("Failed to read mappings {err}")))?;

    files.sort_by(|a,b| a.file_name().cmp(&b.file_name()));

    let mut mappings = vec![];
    for file_path in files {
        match read_mapping(&file_path, resolve_env, false) {
            Ok(Some(mapping)) => mappings.push(mapping),
            Ok(None) => {}
            Err(err) => return create_tuliprox_error_result!(TuliproxErrorKind::Info, "Failed to read mapping file {file_path:?}: {err:?}"),
        }
    }

    if mappings.is_empty() {
        return Ok(None);
    }
    merge_mapping_definitions(mappings)
}

pub fn read_mappings_file(mappings_file: &str, resolve_env: bool) -> Result<Option<MappingsDto>, TuliproxError> {
    let path = PathBuf::from(mappings_file);
    match std::fs::metadata(&path) {
        Ok(metadata) => {
            if metadata.is_file() {
                read_mappings_from_file(&path, resolve_env)
            } else if metadata.is_dir() {
                read_mappings_from_directory(&path, resolve_env)
            } else {
                Ok(None)
            }
        }
        Err(_err) => {
            Ok(None)
        }
    }
}

pub fn read_mappings(mappings_file: &str, resolve_env: bool) -> Result<Option<Mappings>, TuliproxError> {
    match read_mappings_file(mappings_file, resolve_env)? {
        Some(dto) => Ok(Some(Mappings::from(&dto))),
        None => Ok(None),
    }
}