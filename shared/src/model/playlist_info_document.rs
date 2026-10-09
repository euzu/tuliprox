use crate::model::{PlaylistItemType, StreamProperties, VirtualId, XtreamMappingOptions};
use serde::{Deserialize, Serialize};

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
    pub fn to_info_document(
        &self,
        options: &XtreamMappingOptions,
        item_type: PlaylistItemType,
        virtual_id: VirtualId,
        category_id: u32,
    ) -> XtreamInfoDocument {
        match self {
            StreamProperties::Live(_live) => {
                // Live streams don't expose info documents through the Xtream API.
                XtreamInfoDocument::Empty(XtreamEmptyDoc {})
            }
            StreamProperties::Video(video) => XtreamInfoDocument::Video(self.video_to_info_document(
                options,
                video,
                item_type,
                virtual_id,
                category_id,
            )),
            StreamProperties::Series(series) => XtreamInfoDocument::Series(self.series_to_info_document(
                options,
                series,
                item_type,
                virtual_id,
                category_id,
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
