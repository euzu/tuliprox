use crate::model::macros;
use shared::model::{ConfigRenameDto, ItemField};
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct ConfigRename {
    pub field: ItemField,
    pub new_name: String,
    pub raw_pattern: String,
    pub pattern: Arc<regex::Regex>,
}

macros::from_impl!(ConfigRename);
impl From<&ConfigRenameDto> for ConfigRename {
    fn from(dto: &ConfigRenameDto) -> Self {
        let pattern_source = dto.t_pattern.as_ref().unwrap_or(&dto.pattern);
        let pattern = shared::model::REGEX_CACHE
            .get_or_compile(pattern_source)
            .or_else(|err| {
                log::warn!(
                    "Invalid rename regex pattern '{pattern_source}': {err}. Falling back to escaped literal pattern."
                );
                shared::model::REGEX_CACHE.get_or_compile(&regex::escape(pattern_source))
            })
            .unwrap_or_else(|_| {
                // Final fallback that avoids panicking on malformed user input.
                shared::model::REGEX_CACHE.get_or_compile("$^").expect("hardcoded fallback regex '$^' must compile")
            });
        Self { field: dto.field, new_name: dto.new_name.clone(), raw_pattern: dto.pattern.clone(), pattern }
    }
}

impl From<&ConfigRename> for ConfigRenameDto {
    fn from(instance: &ConfigRename) -> Self {
        Self {
            field: instance.field,
            new_name: instance.new_name.clone(),
            pattern: if instance.raw_pattern.is_empty() {
                instance.pattern.to_string()
            } else {
                instance.raw_pattern.clone()
            },
            t_pattern: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_dto_prefers_resolved_pattern() {
        let dto = ConfigRenameDto {
            field: ItemField::Name,
            pattern: "!GROUP_PATTERN!".to_string(),
            new_name: "Renamed".to_string(),
            t_pattern: Some("^US.*$".to_string()),
        };

        let rename = ConfigRename::from(&dto);
        assert_eq!(rename.pattern.as_str(), "^US.*$");
        assert_eq!(rename.raw_pattern, "!GROUP_PATTERN!");
    }

    #[test]
    fn to_dto_preserves_original_placeholder_pattern() {
        let dto = ConfigRenameDto {
            field: ItemField::Name,
            pattern: "!GROUP_PATTERN!".to_string(),
            new_name: "Renamed".to_string(),
            t_pattern: Some("^US.*$".to_string()),
        };
        let rename = ConfigRename::from(&dto);
        let mapped_back = ConfigRenameDto::from(&rename);

        assert_eq!(mapped_back.pattern, "!GROUP_PATTERN!");
        assert_eq!(mapped_back.t_pattern, None);
    }
}
