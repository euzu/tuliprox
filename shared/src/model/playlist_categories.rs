#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default, PartialEq)]
pub struct PlaylistClusterCategoriesDto {
    #[serde(default)]
    pub live: Option<Vec<String>>,
    #[serde(default)]
    pub vod: Option<Vec<String>>,
    #[serde(default)]
    pub series: Option<Vec<String>>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default, PartialEq)]
pub struct PlaylistCategoriesDto {
    #[serde(default)]
    pub xtream: Option<PlaylistClusterCategoriesDto>,
    #[serde(default)]
    pub m3u: Option<PlaylistClusterCategoriesDto>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default, PartialEq)]
pub struct PlaylistClusterBouquetDto {
    #[serde(default)]
    pub live: Option<Vec<String>>,
    #[serde(default)]
    pub vod: Option<Vec<String>>,
    #[serde(default)]
    pub series: Option<Vec<String>>,
}

impl PlaylistClusterBouquetDto {
    pub fn canonicalize_for_target(&mut self) {
        Self::canonicalize_cluster_list(&mut self.live);
        Self::canonicalize_cluster_list(&mut self.vod);
        Self::canonicalize_cluster_list(&mut self.series);
    }

    #[inline]
    pub fn is_target_unrestricted(&self) -> bool {
        self.live.as_ref().is_none_or(Vec::is_empty)
            && self.vod.as_ref().is_none_or(Vec::is_empty)
            && self.series.as_ref().is_none_or(Vec::is_empty)
    }

    fn canonicalize_cluster_list(list: &mut Option<Vec<String>>) {
        if let Some(items) = list {
            items.sort_unstable();
            items.dedup();
            if items.is_empty() {
                *list = None;
            }
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default, PartialEq)]
pub struct PlaylistBouquetDto {
    #[serde(default)]
    pub xtream: Option<PlaylistClusterBouquetDto>,
    #[serde(default)]
    pub m3u: Option<PlaylistClusterBouquetDto>,
}
