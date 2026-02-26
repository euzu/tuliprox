#[derive(Debug, serde::Serialize, serde::Deserialize, Default)]
pub struct UserApiRequest {
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub password: String,
    #[serde(default)]
    pub token: String,
    #[serde(default)]
    pub action: String,
    #[serde(default)]
    pub series_id: String,
    #[serde(default)]
    pub vod_id: String,
    #[serde(default)]
    pub stream_id: String,
    #[serde(default)]
    pub category_id: String,
    #[serde(default)]
    pub limit: String,
    #[serde(default)]
    pub start: String,
    #[serde(default)]
    pub end: String,
    #[serde(default)]
    pub stream: String,
    #[serde(default)]
    pub duration: String,
    #[serde(default, alias = "type")]
    pub content_type: String,
}

impl UserApiRequest {
    pub fn get_limit(&self) -> u32 {
        if self.limit.is_empty() {
            0
        } else {
            self.limit.parse::<u32>().unwrap_or(0)
        }
    }
}
