use crate::utils::traverse_dir;
use crate::utils::{config_file_reader, open_file};
use log::{debug, warn};
use shared::error::{info_err_res, TuliproxError};
use shared::info_err;
use shared::model::TemplateDefinitionDto;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

fn read_template_definition(
    template_file: &Path,
    resolve_var: bool,
) -> Result<Option<TemplateDefinitionDto>, TuliproxError> {
    match open_file(template_file) {
        Ok(file) => {
            let maybe_definition: Result<TemplateDefinitionDto, _> =
                serde_saphyr::from_reader(config_file_reader(file, resolve_var));
            match maybe_definition {
                Ok(definition) => Ok(Some(definition)),
                Err(err) => info_err_res!("{err}"),
            }
        }
        Err(err) => {
            if err.kind() == ErrorKind::NotFound {
                debug!(
                    "Optional template file not found: {}",
                    template_file.to_str().unwrap_or("?")
                );
            } else {
                warn!(
                    "Can't read template file {}: {err}",
                    template_file.to_str().unwrap_or("?")
                );
            }
            Ok(None)
        }
    }
}

fn merge_template_definitions(definitions: Vec<TemplateDefinitionDto>) -> Option<TemplateDefinitionDto> {
    let mut merged = Vec::new();
    for mut definition in definitions {
        merged.append(&mut definition.templates);
    }
    if merged.is_empty() {
        None
    } else {
        Some(TemplateDefinitionDto { templates: merged })
    }
}

fn read_templates_from_file(
    templates_file: &Path,
    resolve_env: bool,
) -> Result<Option<(Vec<PathBuf>, TemplateDefinitionDto)>, TuliproxError> {
    match read_template_definition(templates_file, resolve_env)? {
        Some(definition) => Ok(Some((vec![templates_file.to_path_buf()], definition))),
        None => Ok(None),
    }
}

fn read_templates_from_directory(
    path: &Path,
    resolve_env: bool,
) -> Result<Option<(Vec<PathBuf>, TemplateDefinitionDto)>, TuliproxError> {
    let mut files = vec![];
    let mut visit = |entry: &std::fs::DirEntry, metadata: &std::fs::Metadata| {
        if metadata.is_file() {
            let file_path = entry.path();
            if file_path.extension().is_some_and(|ext| ext == "yml") {
                files.push(file_path);
            }
        }
    };
    traverse_dir(path, &mut visit).map_err(|err| info_err!("Failed to read templates {err}"))?;

    files.sort();

    let mut definitions = vec![];
    let mut loaded_template_files = vec![];
    for file_path in files {
        match read_template_definition(&file_path, resolve_env) {
            Ok(Some(definition)) => {
                loaded_template_files.push(file_path);
                definitions.push(definition);
            }
            Ok(None) => {}
            Err(err) => return info_err_res!("Failed to read template file {file_path:?}: {err:?}"),
        }
    }

    if definitions.is_empty() {
        return Ok(None);
    }

    Ok(merge_template_definitions(definitions).map(|definition| (loaded_template_files, definition)))
}

pub fn read_templates_file(
    templates_file: &str,
    resolve_env: bool,
) -> Result<Option<(Vec<PathBuf>, TemplateDefinitionDto)>, TuliproxError> {
    let path = PathBuf::from(templates_file);
    match std::fs::metadata(&path) {
        Ok(metadata) => {
            if metadata.is_file() {
                read_templates_from_file(&path, resolve_env)
            } else if metadata.is_dir() {
                read_templates_from_directory(&path, resolve_env)
            } else {
                warn!(
                    "Template path exists but is neither file nor directory: {}",
                    path.to_string_lossy()
                );
                Ok(None)
            }
        }
        Err(err) => {
            if err.kind() == ErrorKind::NotFound {
                debug!("Optional template path not found: {}", path.to_string_lossy());
            } else {
                warn!(
                    "Can't read template path metadata for {}: {err}",
                    path.to_string_lossy()
                );
            }
            Ok(None)
        }
    }
}
