#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
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
    pub fn merge_prefer_primary(primary: &Self, fallback: &Self) -> Self {
        fn pick(primary: &str, fallback: &str) -> String {
            if primary.is_empty() {
                fallback.to_string()
            } else {
                primary.to_string()
            }
        }

        Self {
            username: pick(&primary.username, &fallback.username),
            password: pick(&primary.password, &fallback.password),
            token: pick(&primary.token, &fallback.token),
            action: pick(&primary.action, &fallback.action),
            series_id: pick(&primary.series_id, &fallback.series_id),
            vod_id: pick(&primary.vod_id, &fallback.vod_id),
            stream_id: pick(&primary.stream_id, &fallback.stream_id),
            category_id: pick(&primary.category_id, &fallback.category_id),
            limit: pick(&primary.limit, &fallback.limit),
            start: pick(&primary.start, &fallback.start),
            end: pick(&primary.end, &fallback.end),
            stream: pick(&primary.stream, &fallback.stream),
            duration: pick(&primary.duration, &fallback.duration),
            content_type: pick(&primary.content_type, &fallback.content_type),
        }
    }

    pub fn get_limit(&self) -> u32 {
        if self.limit.is_empty() {
            0
        } else {
            self.limit.parse::<u32>().unwrap_or(0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::UserApiRequest;

    #[test]
    fn merge_prefer_primary_uses_fallback_for_empty_fields() {
        let primary = UserApiRequest {
            username: String::new(),
            password: String::new(),
            action: String::from("get_live_categories"),
            ..UserApiRequest::default()
        };
        let fallback = UserApiRequest {
            username: String::from("xtr"),
            password: String::from("xtr"),
            ..UserApiRequest::default()
        };

        let merged = UserApiRequest::merge_prefer_primary(&primary, &fallback);

        assert_eq!(merged.username, "xtr");
        assert_eq!(merged.password, "xtr");
        assert_eq!(merged.action, "get_live_categories");
    }
}
