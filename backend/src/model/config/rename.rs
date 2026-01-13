use std::sync::Arc;
use shared::model::{ConfigRenameDto, ItemField};
use crate::model::macros;

#[derive(Debug, Clone)]
pub struct ConfigRename {
    pub field: ItemField,
    pub new_name: String,
    pub pattern: Arc<regex::Regex>,
}

macros::from_impl!(ConfigRename);
impl From<&ConfigRenameDto> for ConfigRename {
    fn from(dto: &ConfigRenameDto) -> Self {
        Self {
            field: dto.field,
            new_name: dto.new_name.clone(),
            pattern: shared::model::REGEX_CACHE.get_or_compile(&dto.pattern).unwrap_or_else(|_| panic!("Invalid regex pattern {}", dto.pattern)),
        }
    }
}

impl From<&ConfigRename> for ConfigRenameDto {
    fn from(instance: &ConfigRename) -> Self {
        Self {
            field: instance.field,
            new_name: instance.new_name.clone(),
            pattern: instance.pattern.to_string()
        }
    }
}