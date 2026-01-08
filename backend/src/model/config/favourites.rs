use std::sync::Arc;
use crate::model::macros;
use regex::Regex;
use shared::foundation::filter::{CompiledRegex, Filter};
use shared::model::{ConfigFavouritesDto, ItemField};

#[derive(Debug, Clone)]
pub struct ConfigFavourites {
    pub group: Arc<str>,
    pub filter: Filter,
    pub match_as_ascii: bool,
}

impl ConfigFavourites {
    fn default_filter() -> Filter {
        Filter::FieldComparison(
            ItemField::Group,
            CompiledRegex {
                restr: String::new(),
                re: Regex::new(".*").unwrap(),
            },
        )
    }
}


macros::from_impl!(ConfigFavourites);
impl From<&ConfigFavouritesDto> for ConfigFavourites {
    fn from(dto: &ConfigFavouritesDto) -> Self {
        Self {
            group: dto.group.clone(),
            filter: dto.t_filter.as_ref().map_or_else(Self::default_filter, Clone::clone),
            match_as_ascii: dto.match_as_ascii,
        }
    }
}

impl From<&ConfigFavourites> for ConfigFavouritesDto {
    fn from(instance: &ConfigFavourites) -> Self {
        Self {
            group: instance.group.clone(),
            filter: instance.filter.to_string(),
            match_as_ascii: instance.match_as_ascii,
            t_filter: Some(instance.filter.clone()),
        }
    }
}