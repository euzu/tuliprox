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

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default, PartialEq)]
pub struct PlaylistBouquetDto {
    #[serde(default)]
    pub xtream: Option<PlaylistClusterBouquetDto>,
    #[serde(default)]
    pub m3u: Option<PlaylistClusterBouquetDto>,
}
