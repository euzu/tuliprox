use crate::model::macros;
use shared::{
    foundation::Filter,
    model::{ConfigFavouritesDto, XtreamCluster},
};
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct ConfigFavourites {
    pub cluster: XtreamCluster,
    pub group: Arc<str>,
    pub filter: Filter,
    pub match_as_ascii: bool,
}

macros::from_impl!(ConfigFavourites);
impl From<&ConfigFavouritesDto> for ConfigFavourites {
    fn from(dto: &ConfigFavouritesDto) -> Self {
        Self {
            cluster: dto.cluster,
            group: dto.group.clone(),
            filter: dto.t_filter.as_ref().map_or_else(Filter::default, Clone::clone),
            match_as_ascii: dto.match_as_ascii,
        }
    }
}

impl From<&ConfigFavourites> for ConfigFavouritesDto {
    fn from(instance: &ConfigFavourites) -> Self {
        Self {
            cluster: instance.cluster,
            group: instance.group.clone(),
            filter: instance.filter.to_string(),
            match_as_ascii: instance.match_as_ascii,
            t_filter: Some(instance.filter.clone()),
        }
    }
}
