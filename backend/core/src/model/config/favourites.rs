use crate::model::macros;
use shared::{
    foundation::{CompiledRegex, Filter, FilterBuilder, Leaf, PresenceOperator},
    model::{ConfigFavouritesDto, ItemField, XtreamCluster},
};
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct ConfigFavourites {
    pub cluster: XtreamCluster,
    pub group: Arc<str>,
    pub filter: Filter,
    pub match_as_ascii: bool,
}

impl ConfigFavourites {
    /// A filter that accepts every group.
    fn default_filter() -> Filter {
        let mut builder = FilterBuilder::new();
        let root = match shared::model::REGEX_CACHE.get_or_compile(".*") {
            Ok(re) => builder.leaf(Leaf::FieldComparison(ItemField::Group, CompiledRegex { restr: String::new(), re })),
            // `.*` is a valid pattern, so this means the regex engine or its cache is
            // unusable. Fall back to a match-all expression that needs no regex at all
            // instead of aborting the process.
            Err(err) => {
                log::error!("cannot compile the default favourites filter regex '.*': {err}");
                let empty = builder.leaf(Leaf::PresenceComparison(ItemField::Group, PresenceOperator::IsEmpty));
                let present = builder.leaf(Leaf::PresenceComparison(ItemField::Group, PresenceOperator::IsNotEmpty));
                builder.or(empty, present)
            }
        };
        builder.finish(root)
    }
}

macros::from_impl!(ConfigFavourites);
impl From<&ConfigFavouritesDto> for ConfigFavourites {
    fn from(dto: &ConfigFavouritesDto) -> Self {
        Self {
            cluster: dto.cluster,
            group: dto.group.clone(),
            filter: dto.t_filter.as_ref().map_or_else(Self::default_filter, Clone::clone),
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
