use std::sync::Arc;
use crate::error::{TuliproxError};
use crate::foundation::filter::{get_filter, Filter};
use crate::model::{PatternTemplate};
use crate::utils::arc_str_serde;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ConfigFavouritesDto {
    #[serde(with = "arc_str_serde")]
    pub group: Arc<str>,
    #[serde(default)]
    pub filter: String,
    #[serde(default)]
    pub match_as_ascii: bool,
    #[serde(skip)]
    pub t_filter: Option<Filter>,
}

impl ConfigFavouritesDto {
    pub fn prepare(&mut self, templates: Option<&Vec<PatternTemplate>>) -> Result<(), TuliproxError> {
        self.t_filter = Some(get_filter(&self.filter, templates)?);
        Ok(())
    }
}