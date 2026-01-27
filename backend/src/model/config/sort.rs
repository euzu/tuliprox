use std::sync::Arc;
use regex::Regex;
use shared::model::{ConfigSortRuleDto, ConfigSortDto, ItemField, SortOrder, SortTarget};
use shared::foundation::Filter;
use crate::model::macros;



#[derive(Debug, Clone)]
pub struct ConfigSortRule {
    pub target: SortTarget,
    pub order: SortOrder,
    pub field: ItemField,
    pub sequence: Option<Vec<Arc<Regex>>>,
    pub filter: Filter,
}

macros::from_impl!(ConfigSortRule);
impl From<&ConfigSortRuleDto> for ConfigSortRule {
    fn from(dto: &ConfigSortRuleDto) -> Self {
        Self {
            target: dto.target,
            order: dto.order,
            field: dto.field,
            sequence: dto.t_sequence.clone(),
            filter: dto.t_filter.clone().unwrap_or_default(),
        }
    }
}

impl From<&ConfigSortRule> for ConfigSortRuleDto {
    fn from(instance: &ConfigSortRule) -> Self {
        Self {
            target: instance.target,
            order: instance.order,
            field: instance.field,
            sequence: instance.sequence.as_ref().map(|l: &Vec<Arc<Regex>>| l.iter().map(ToString::to_string).collect()),
            filter: instance.filter.to_string(),
            t_sequence: None,
            t_filter: Some(instance.filter.clone()),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct ConfigSort {
    pub match_as_ascii: bool,
    pub rules: Vec<ConfigSortRule>,
}

macros::from_impl!(ConfigSort);
impl From<&ConfigSortDto> for ConfigSort {
    fn from(dto: &ConfigSortDto) -> Self {
        Self {
            match_as_ascii: dto.match_as_ascii,
            rules: dto.rules.iter().map(Into::into).collect(),
        }
    }
}

impl From<&ConfigSort> for ConfigSortDto {
    fn from(instance: &ConfigSort) -> Self {
        Self {
            match_as_ascii: instance.match_as_ascii,
            rules: instance.rules.iter().map(Into::into).collect(),
        }
    }
}
