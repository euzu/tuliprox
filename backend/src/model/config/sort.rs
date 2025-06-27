use regex::Regex;
use shared::model::{ConfigSortChannelDto, ConfigSortDto, ConfigSortGroupDto, ItemField, SortOrder};
use crate::model::macros;

#[derive(Debug, Clone)]
pub struct ConfigSortGroup {
    pub order: SortOrder,
    pub sequence: Option<Vec<Regex>>,
}

macros::from_impl!(ConfigSortGroup);
impl From<&ConfigSortGroupDto> for ConfigSortGroup {
    fn from(dto: &ConfigSortGroupDto) -> Self {
        Self {
            order: dto.order,
            sequence: dto.t_sequence.clone(),
        }
    }
}

impl From<&ConfigSortGroup> for ConfigSortGroupDto {
    fn from(instance: &ConfigSortGroup) -> Self {
        Self {
            order: instance.order,
            sequence: instance.sequence.as_ref().map(|l| l.iter().map(ToString::to_string).collect()),
            t_sequence: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ConfigSortChannel {
    pub field: ItemField,
    pub group_pattern: Regex,
    pub order: SortOrder,
    pub sequence: Option<Vec<Regex>>,
}

macros::from_impl!(ConfigSortChannel);
impl From<&ConfigSortChannelDto> for ConfigSortChannel {
    fn from(dto: &ConfigSortChannelDto) -> Self {
        Self {
            field: dto.field,
            group_pattern: Regex::new(&dto.group_pattern).unwrap(),
            order: dto.order,
            sequence: dto.t_sequence.clone(),
        }
    }
}

impl From<&ConfigSortChannel> for ConfigSortChannelDto {
    fn from(instance: &ConfigSortChannel) -> Self {
        Self {
            field: instance.field,
            group_pattern: instance.group_pattern.to_string(),
            order: instance.order,
            sequence: instance.sequence.as_ref().map(|l| l.iter().map(ToString::to_string).collect()),
            t_sequence: None,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct ConfigSort {
    pub match_as_ascii: bool,
    pub groups: Option<ConfigSortGroup>,
    pub channels: Option<Vec<ConfigSortChannel>>,
}

macros::from_impl!(ConfigSort);
impl From<&ConfigSortDto> for ConfigSort {
    fn from(dto: &ConfigSortDto) -> Self {
        Self {
            match_as_ascii: dto.match_as_ascii,
            groups: dto.groups.as_ref().map(Into::into),
            channels: dto.channels.as_ref().map(|v| v.iter().map(Into::into).collect()),
        }
    }
}

impl From<&ConfigSort> for ConfigSortDto {
    fn from(instance: &ConfigSort) -> Self {
        Self {
            match_as_ascii: instance.match_as_ascii,
            groups: instance.groups.as_ref().map(Into::into),
            channels: instance.channels.as_ref().map(|v| v.iter().map(Into::into).collect()),
        }
    }
}
