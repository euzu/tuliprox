use crate::{
    model::{PlaylistItemType, StreamProperties, VirtualId, XtreamMappingOptions},
    utils::{resolve_container_extension, Internable},
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[allow(clippy::large_enum_variant)]
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(untagged)]
pub enum XtreamInfoDocument {
    Video(XtreamVideoInfoDoc),
    Series(XtreamSeriesInfoDoc),
    Empty(XtreamEmptyDoc),
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
pub struct XtreamEmptyDoc {}

impl Default for XtreamInfoDocument {
    fn default() -> Self { Self::Empty(XtreamEmptyDoc {}) }
}

impl StreamProperties {
    /// Builds the info document for one item.
    ///
    /// `item_url` is the URL of the item whose document is built. It carries the stream extension when
    /// the provider omitted one, so it is part of the container-extension resolution of a movie.
    pub fn to_info_document(
        &self,
        options: &XtreamMappingOptions,
        item_type: PlaylistItemType,
        virtual_id: VirtualId,
        category_id: u32,
        item_url: &str,
        configured_extension: Option<&Arc<str>>,
    ) -> XtreamInfoDocument {
        match self {
            StreamProperties::Live(_live) => {
                // Live streams don't expose info documents through the Xtream API.
                XtreamInfoDocument::Empty(XtreamEmptyDoc {})
            }
            StreamProperties::Video(video) => {
                let mut document = self.video_to_info_document(options, video, item_type, virtual_id, category_id);
                // `StreamProperties` carries no URL, so the provider value is normalized and a blank one
                // is filled here, where the item URL and the input's configured default are known.
                // Without this, a movie served from cached provider details publishes an empty or
                // placeholder extension.
                document.movie_data.container_extension = resolve_container_extension(
                    &document.movie_data.container_extension,
                    item_url,
                    configured_extension.map(Arc::as_ref),
                )
                .as_ref()
                .intern();
                XtreamInfoDocument::Video(document)
            }
            StreamProperties::Series(series) => XtreamInfoDocument::Series(self.series_to_info_document(
                options,
                series,
                item_type,
                virtual_id,
                category_id,
                configured_extension,
            )),
            StreamProperties::Episode(_episode) => {
                // Episode streams don't expose info documents through the Xtream API.
                XtreamInfoDocument::Empty(XtreamEmptyDoc {})
            }
        }
    }
}

#[cfg(test)]
mod tests;

mod series;
mod video;

pub use series::{
    XtreamSeriesEpisodeInfoData, XtreamSeriesEpisodeInfoDoc, XtreamSeriesInfoData, XtreamSeriesInfoDoc,
    XtreamSeriesSeasonInfoDoc,
};
pub use video::{XtreamVideoInfoData, XtreamVideoInfoDoc, XtreamVideoMovieData};
