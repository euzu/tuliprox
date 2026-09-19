use crate::{
    model::CompiledMappings,
    utils::{config_file_reader, open_file},
};
use log::warn;
use shared::{
    error::TuliproxError,
    model::{MappingDefinitionDto, MappingDto, MappingsDto, PatternTemplate, Prepare},
};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

fn read_mapping(mapping_file: &Path, resolve_var: bool) -> Result<Option<MappingsDto>, TuliproxError> {
    if let Ok(file) = open_file(mapping_file) {
        let maybe_mapping: Result<MappingsDto, _> = serde_saphyr::from_reader(config_file_reader(file, resolve_var));
        return match maybe_mapping {
            Ok(mapping) => Ok(Some(mapping)),
            Err(err) => Err(TuliproxError::Config(format!("{err}"))),
        };
    }
    warn!("Can't read mapping file: {}", mapping_file.to_str().unwrap_or("?"));
    Ok(None)
}

fn read_mappings_from_file(
    mappings_file: &Path,
    resolve_env: bool,
) -> Result<Option<(Vec<PathBuf>, MappingsDto)>, TuliproxError> {
    match read_mapping(mappings_file, resolve_env) {
        Ok(mappings) => match mappings {
            None => Ok(None),
            Some(mappings_cfg) => Ok(Some((vec![mappings_file.to_path_buf()], mappings_cfg))),
        },
        Err(err) => Err(err),
    }
}

fn merge_mappings(mappings: Vec<(PathBuf, MappingDto)>) -> Result<Vec<MappingDto>, TuliproxError> {
    let mut result: Vec<MappingDto> = Vec::with_capacity(mappings.len());
    let mut index_by_id: HashMap<String, usize> = HashMap::with_capacity(mappings.len());
    let mut source_by_index: Vec<PathBuf> = Vec::with_capacity(mappings.len());

    for (source, mut m) in mappings {
        if let Some(&idx) = index_by_id.get(&m.id) {
            let entry = &mut result[idx];
            if entry.stage != m.stage {
                return Err(TuliproxError::Config(format!(
                    "Mapping '{}' has conflicting stages '{}' in '{}' and '{}' in '{}'",
                    m.id,
                    entry.stage,
                    source_by_index[idx].display(),
                    m.stage,
                    source.display()
                )));
            }
            if entry.match_as_ascii != m.match_as_ascii {
                warn!(
                    "Mapping '{}' has conflicting match_as_ascii values in '{}' and '{}'; keeping the first value ({})",
                    m.id,
                    source_by_index[idx].display(),
                    source.display(),
                    entry.match_as_ascii
                );
            }
            if let Some(mut mapper) = m.mapper.take() {
                entry.mapper.get_or_insert_with(Vec::new).append(&mut mapper);
            }
            if let Some(mut counters) = m.counter.take() {
                entry.counter.get_or_insert_with(Vec::new).append(&mut counters);
            }
        } else {
            index_by_id.insert(m.id.clone(), result.len());
            result.push(m);
            source_by_index.push(source);
        }
    }

    Ok(result)
}

fn merge_mapping_definitions(mappings: Vec<(PathBuf, MappingsDto)>) -> Result<MappingsDto, TuliproxError> {
    let mut merged_templates: Vec<PatternTemplate> = Vec::new();
    let mut merged_mapping: Vec<(PathBuf, MappingDto)> = Vec::new();

    for (source, mapping) in mappings {
        if let Some(mut templates) = mapping.mappings.templates {
            merged_templates.append(&mut templates);
        }

        merged_mapping.extend(mapping.mappings.mapping.into_iter().map(|mapping| (source.clone(), mapping)));
    }

    let mapping = merge_mappings(merged_mapping)?;
    Ok(MappingsDto {
        mappings: MappingDefinitionDto {
            templates: if merged_templates.is_empty() { None } else { Some(merged_templates) },
            mapping,
        },
    })
}

fn read_mappings_from_directory(
    path: &Path,
    resolve_env: bool,
) -> Result<Option<(Vec<PathBuf>, MappingsDto)>, TuliproxError> {
    let mut files = crate::utils::collect_yaml_files(path)
        .map_err(|err| TuliproxError::Io(format!("Failed to read mappings {err}")))?;

    files.sort_by(|a, b| a.file_name().cmp(&b.file_name()));

    let mut mappings = vec![];
    let mut loaded_mapping_files = vec![];
    for file_path in files {
        match read_mapping(&file_path, resolve_env) {
            Ok(Some(mapping)) => {
                mappings.push((file_path.clone(), mapping));
                loaded_mapping_files.push(file_path);
            }
            Ok(None) => {}
            Err(err) => {
                return Err(TuliproxError::Io(format!("Failed to read mapping file {}: {err}", file_path.display())))
            }
        }
    }

    if mappings.is_empty() {
        return Ok(None);
    }
    Ok(Some((loaded_mapping_files, merge_mapping_definitions(mappings)?)))
}

pub fn read_mappings_file_unprepared(
    mappings_file: &str,
    resolve_env: bool,
) -> Result<Option<(Vec<PathBuf>, MappingsDto)>, TuliproxError> {
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
        Err(_err) => Ok(None),
    }
}

pub fn read_mappings_file(
    mappings_file: &str,
    resolve_env: bool,
) -> Result<Option<(Vec<PathBuf>, MappingsDto)>, TuliproxError> {
    read_mappings_file_with_templates(mappings_file, resolve_env, None)
}

pub fn read_mappings_file_with_templates(
    mappings_file: &str,
    resolve_env: bool,
    prepared_templates: Option<&[PatternTemplate]>,
) -> Result<Option<(Vec<PathBuf>, MappingsDto)>, TuliproxError> {
    let maybe_result = read_mappings_file_unprepared(mappings_file, resolve_env)?;

    if let Some((paths, mut dto)) = maybe_result {
        dto.mappings.prepare(prepared_templates)?;
        Ok(Some((paths, dto)))
    } else {
        Ok(None)
    }
}

pub fn read_mappings(
    mappings_file: &str,
    resolve_env: bool,
) -> Result<Option<(Vec<PathBuf>, CompiledMappings)>, TuliproxError> {
    match read_mappings_file(mappings_file, resolve_env)? {
        Some((paths, dto)) => CompiledMappings::try_from(&dto).map(|mappings| Some((paths, mappings))),
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared::model::{CounterModifier, MapperDto, MappingStage};
    use tempfile::tempdir;

    fn dto(id: &str, stage: MappingStage, mappers: Vec<&str>, counters: usize) -> MappingDto {
        MappingDto {
            id: id.to_string(),
            match_as_ascii: false,
            stage,
            mapper: if mappers.is_empty() {
                None
            } else {
                Some(
                    mappers
                        .into_iter()
                        .map(|script| MapperDto {
                            filter: String::new(),
                            script: script.to_string(),
                            ..Default::default()
                        })
                        .collect(),
                )
            },
            counter: (counters > 0).then(|| {
                (0..counters)
                    .map(|i| shared::model::MappingCounterDefinition {
                        filter: String::new(),
                        field: "name".to_string(),
                        concat: format!("c{i}"),
                        modifier: CounterModifier::Assign,
                        value: 0,
                        padding: 0,
                    })
                    .collect()
            }),
            t_counter: None,
            templates: None,
        }
    }

    fn definition(mappings: Vec<MappingDto>) -> MappingsDto {
        MappingsDto { mappings: MappingDefinitionDto { templates: None, mapping: mappings } }
    }

    fn sources(definitions: Vec<MappingsDto>) -> Vec<(PathBuf, MappingsDto)> {
        definitions
            .into_iter()
            .enumerate()
            .map(|(index, definition)| (PathBuf::from(format!("mapping-{index}.yml")), definition))
            .collect()
    }

    #[test]
    fn merge_rejects_conflicting_stages_for_same_id() {
        let first = definition(vec![dto("a", MappingStage::Processing, vec![], 0)]);
        let second = definition(vec![dto("a", MappingStage::AfterEpg, vec![], 0)]);
        let err = merge_mapping_definitions(sources(vec![first, second])).expect_err("conflict must error");
        let msg = format!("{err}");
        assert!(msg.contains("'a'"), "message should mention the id: {msg}");
        assert!(msg.contains("processing"), "message should mention the first stage: {msg}");
        assert!(msg.contains("after_epg"), "message should mention the conflicting stage: {msg}");
    }

    #[test]
    fn directory_stage_conflict_reports_both_source_files() {
        let dir = tempdir().expect("tempdir");
        let first_path = dir.path().join("first.yml");
        let second_path = dir.path().join("second.yml");
        std::fs::write(&first_path, "mappings:\n  mapping:\n    - id: duplicate\n      stage: processing\n")
            .expect("write first mapping");
        std::fs::write(&second_path, "mappings:\n  mapping:\n    - id: duplicate\n      stage: after_epg\n")
            .expect("write second mapping");

        let error = read_mappings_from_directory(dir.path(), false).expect_err("conflict must fail");
        let message = error.to_string();
        assert!(message.contains(&first_path.display().to_string()), "missing first path: {message}");
        assert!(message.contains(&second_path.display().to_string()), "missing second path: {message}");
    }

    #[test]
    fn merge_appends_mapper_and_counter_lists_for_same_stage() {
        let first = definition(vec![dto("a", MappingStage::Processing, vec!["first"], 1)]);
        let second = definition(vec![dto("a", MappingStage::Processing, vec!["second"], 1)]);
        let merged = merge_mapping_definitions(sources(vec![first, second])).expect("same stage must merge");
        let mapping = &merged.mappings.mapping;
        assert_eq!(mapping.len(), 1, "same id must collapse");
        assert_eq!(mapping[0].mapper.as_ref().map(Vec::len), Some(2));
        assert_eq!(mapping[0].counter.as_ref().map(Vec::len), Some(2));
    }

    #[test]
    fn merge_preserves_first_block_fields_including_stage_and_match_as_ascii() {
        let mut a = dto("a", MappingStage::AfterEpg, vec!["first"], 0);
        a.match_as_ascii = true;
        let b = dto("a", MappingStage::AfterEpg, vec!["second"], 0);
        let merged = merge_mapping_definitions(sources(vec![definition(vec![a]), definition(vec![b])]))
            .expect("same stage must merge");
        let mapping = &merged.mappings.mapping;
        assert_eq!(mapping.len(), 1);
        assert!(mapping[0].match_as_ascii, "first block's match_as_ascii must be retained");
        assert_eq!(mapping[0].stage, MappingStage::AfterEpg);
    }

    #[test]
    fn merge_preserves_declaration_order_for_distinct_ids() {
        let first = definition(vec![dto("b", MappingStage::Processing, vec![], 0)]);
        let second = definition(vec![dto("a", MappingStage::Processing, vec![], 0)]);
        let third = definition(vec![dto("c", MappingStage::Processing, vec![], 0)]);
        let merged = merge_mapping_definitions(sources(vec![first, second, third])).expect("distinct ids must merge");
        let ids: Vec<&str> = merged.mappings.mapping.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["b", "a", "c"]);
    }
}
