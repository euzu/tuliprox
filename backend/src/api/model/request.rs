use axum::extract::{FromRequest, Request};
use axum::response::Response;
use log::log_enabled;

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

/// Custom extractor that parses `UserApiRequest` from query parameters and request body
/// (either `application/x-www-form-urlencoded` or `multipart/form-data`), then merges
/// them with query parameters taking priority over body fields.
pub struct UserApiRequestQueryOrBody(pub UserApiRequest);

impl<S> FromRequest<S> for UserApiRequestQueryOrBody
where
    S: Send + Sync,
{
    type Rejection = Response;

    async fn from_request(req: Request, _state: &S) -> Result<Self, Self::Rejection> {
        let (parts, body) = req.into_parts();

        // Parse query parameters (ignore errors, default to empty)
        let query_req: UserApiRequest = parts
            .uri
            .query()
            .and_then(|q| serde_html_form::from_str(q).ok())
            .unwrap_or_default();

        let content_type = parts
            .headers
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");

        // Parse body (ignore errors, fallback to query-only)
        let body_req = parse_body(body, content_type).await.ok();

        Ok(UserApiRequestQueryOrBody(UserApiRequest::merge_query_over_form(&query_req, body_req.as_ref())))
    }
}

async fn parse_body(body: axum::body::Body, content_type: &str) -> Result<UserApiRequest, String> {
    let bytes = axum::body::to_bytes(body, usize::MAX)
        .await
        .map_err(|e| format!("Failed to read body: {e}"))?;

    if content_type.starts_with("multipart/form-data") {
        parse_multipart_body(&bytes, content_type)
    } else {
        // Treat as form-urlencoded (works for both explicit content-type and missing content-type)
        serde_html_form::from_bytes(&bytes).map_err(|e| format!("Failed to parse form: {e}"))
    }
}

fn parse_multipart_body(bytes: &[u8], content_type: &str) -> Result<UserApiRequest, String> {
    let boundary = content_type
        .split("boundary=")
        .nth(1)
        .ok_or("Missing boundary in multipart content type")?
        .trim();

    let data = std::str::from_utf8(bytes).map_err(|e| format!("Invalid UTF-8: {e}"))?;

    let mut request = UserApiRequest::default();
    let delimiter = format!("--{boundary}");
    for part in data.split(&delimiter) {
        if let Some(header_end) = part.find("\r\n\r\n") {
            let headers = &part[..header_end];
            let body = &part[header_end + 4..];

            if let Some(name_start) = headers.find("name=\"") {
                let name_start = name_start + 6;
                if let Some(name_end) = headers[name_start..].find('\"') {
                    let name = &headers[name_start..name_start + name_end];
                    let value = body.trim().trim_end_matches("\r\n").trim();
                    match name {
                        "username" => request.username = value.to_string(),
                        "password" => request.password = value.to_string(),
                        "token" => request.token = value.to_string(),
                        "action" => request.action = value.to_string(),
                        "series_id" => request.series_id = value.to_string(),
                        "vod_id" => request.vod_id = value.to_string(),
                        "stream_id" => request.stream_id = value.to_string(),
                        "category_id" => request.category_id = value.to_string(),
                        "limit" => request.limit = value.to_string(),
                        "start" => request.start = value.to_string(),
                        "end" => request.end = value.to_string(),
                        "stream" => request.stream = value.to_string(),
                        "duration" => request.duration = value.to_string(),
                        "type" => request.content_type = value.to_string(),
                        _ => {}
                    }
                }
            }
        }
    }
    Ok(request)
}

impl UserApiRequest {
    pub fn merge_prefer_primary(primary: &Self, fallback: &Self) -> Self {
        fn pick(primary: &str, fallback: &str) -> String {
            if primary.trim().is_empty() {
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

    pub fn merge_query_over_form(query: &Self, form: Option<&Self>) -> Self {
        form.map_or_else(|| query.clone(), |form_req| Self::merge_prefer_primary(query, form_req))
    }

    pub fn log_sanitized(&self, endpoint: &str) {
        if log_enabled!(log::Level::Debug) {
            use std::fmt::Write;
            let mut msg = endpoint.to_string();
            if !self.username.is_empty() { let _ = write!(msg, " username={}", self.username); }
            if !self.password.is_empty() { msg.push_str(" password=***"); }
            if !self.token.is_empty() { msg.push_str(" token=***"); }
            if !self.action.is_empty() { let _ = write!(msg, " action={}", self.action); }
            if !self.series_id.is_empty() { let _ = write!(msg, " series_id={}", self.series_id); }
            if !self.vod_id.is_empty() { let _ = write!(msg, " vod_id={}", self.vod_id); }
            if !self.stream_id.is_empty() { let _ = write!(msg, " stream_id={}", self.stream_id); }
            if !self.category_id.is_empty() { let _ = write!(msg, " category_id={}", self.category_id); }
            if !self.limit.is_empty() { let _ = write!(msg, " limit={}", self.limit); }
            if !self.start.is_empty() { let _ = write!(msg, " start={}", self.start); }
            if !self.end.is_empty() { let _ = write!(msg, " end={}", self.end); }
            if !self.stream.is_empty() { let _ = write!(msg, " stream={}", self.stream); }
            if !self.duration.is_empty() { let _ = write!(msg, " duration={}", self.duration); }
            if !self.content_type.is_empty() { let _ = write!(msg, " type={}", self.content_type); }
            log::debug!("{msg}");
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

    #[test]
    fn merge_prefer_primary_uses_fallback_for_whitespace_only_fields() {
        let primary = UserApiRequest {
            username: String::from("   "),
            password: String::from("\t"),
            token: String::from("\n"),
            action: String::from("  "),
            series_id: String::from(" "),
            vod_id: String::from(" "),
            stream_id: String::from(" "),
            category_id: String::from(" "),
            limit: String::from(" "),
            start: String::from(" "),
            end: String::from(" "),
            stream: String::from(" "),
            duration: String::from(" "),
            content_type: String::from(" "),
        };
        let fallback = UserApiRequest {
            username: String::from("user"),
            password: String::from("pass"),
            token: String::from("token"),
            action: String::from("action"),
            series_id: String::from("series"),
            vod_id: String::from("vod"),
            stream_id: String::from("stream"),
            category_id: String::from("category"),
            limit: String::from("10"),
            start: String::from("100"),
            end: String::from("200"),
            stream: String::from("300"),
            duration: String::from("60"),
            content_type: String::from("m3u_plus"),
        };

        let merged = UserApiRequest::merge_prefer_primary(&primary, &fallback);

        assert_eq!(merged.username, "user");
        assert_eq!(merged.password, "pass");
        assert_eq!(merged.token, "token");
        assert_eq!(merged.action, "action");
        assert_eq!(merged.series_id, "series");
        assert_eq!(merged.vod_id, "vod");
        assert_eq!(merged.stream_id, "stream");
        assert_eq!(merged.category_id, "category");
        assert_eq!(merged.limit, "10");
        assert_eq!(merged.start, "100");
        assert_eq!(merged.end, "200");
        assert_eq!(merged.stream, "300");
        assert_eq!(merged.duration, "60");
        assert_eq!(merged.content_type, "m3u_plus");
    }

    #[test]
    fn merge_query_over_form_prefers_query_fields() {
        let query = UserApiRequest {
            username: String::from("query-user"),
            token: String::from("query-token"),
            action: String::from("query-action"),
            ..UserApiRequest::default()
        };
        let form = UserApiRequest {
            username: String::from("form-user"),
            token: String::from("form-token"),
            action: String::from("form-action"),
            ..UserApiRequest::default()
        };

        let merged = UserApiRequest::merge_query_over_form(&query, Some(&form));

        assert_eq!(merged.username, "query-user");
        assert_eq!(merged.token, "query-token");
        assert_eq!(merged.action, "query-action");
    }
}
