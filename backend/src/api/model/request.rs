use axum::extract::{FromRequest, Request};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use http_body_util::LengthLimitError;
use log::log_enabled;
use std::error::Error as StdError;

const MAX_BODY_SIZE_BYTES: usize = 10 * 1024 * 1024;

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

#[derive(Debug)]
enum ParseBodyError {
    PayloadTooLarge(String),
    BadRequest(String),
}

impl ParseBodyError {
    fn into_response(self) -> Response {
        match self {
            Self::PayloadTooLarge(err) => (StatusCode::PAYLOAD_TOO_LARGE, err).into_response(),
            Self::BadRequest(err) => (StatusCode::BAD_REQUEST, err).into_response(),
        }
    }
}

impl<S> FromRequest<S> for UserApiRequestQueryOrBody
where
    S: Send + Sync,
{
    type Rejection = Response;

    async fn from_request(req: Request, _state: &S) -> Result<Self, Self::Rejection> {
        let (parts, body) = req.into_parts();

        let query_req = parse_query_request(parts.uri.query())
            .map_err(ParseBodyError::into_response)?;

        let content_type = parts
            .headers
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");

        let body_req = match parse_body(body, content_type).await {
            Ok(body_req) => Some(body_req),
            Err(err) => return Err(err.into_response()),
        };

        Ok(UserApiRequestQueryOrBody(UserApiRequest::merge_query_over_form(&query_req, body_req.as_ref())))
    }
}

async fn parse_body(body: axum::body::Body, content_type: &str) -> Result<UserApiRequest, ParseBodyError> {
    let bytes = axum::body::to_bytes(body, MAX_BODY_SIZE_BYTES)
        .await
        .map_err(|e| {
            if is_length_limit_error(&e) {
                ParseBodyError::PayloadTooLarge(format!("Request body too large (max {MAX_BODY_SIZE_BYTES} bytes)"))
            } else {
                ParseBodyError::BadRequest(format!("Failed to read request body: {e}"))
            }
        })?;

    if bytes.is_empty() {
        return Ok(UserApiRequest::default());
    }

    if is_multipart_content_type(content_type) {
        parse_multipart_body(&bytes, content_type)
    } else {
        // Treat as form-urlencoded (works for both explicit content-type and missing content-type)
        serde_html_form::from_bytes(&bytes).map_err(|e| ParseBodyError::BadRequest(format!("Failed to parse form: {e}")))
    }
}

fn parse_multipart_body(bytes: &[u8], content_type: &str) -> Result<UserApiRequest, ParseBodyError> {
    let boundary = extract_multipart_boundary(content_type)
        .ok_or_else(|| ParseBodyError::BadRequest("Missing boundary in multipart content type".to_string()))?;

    let data = std::str::from_utf8(bytes).map_err(|e| ParseBodyError::BadRequest(format!("Invalid UTF-8: {e}")))?;

    let mut request = UserApiRequest::default();
    let delimiter = format!("--{boundary}");
    for part in data.split(&delimiter) {
        if let Some((name, value)) = parse_multipart_field(part) {
            request.set_field(name, value);
        }
    }
    Ok(request)
}

fn parse_query_request(query: Option<&str>) -> Result<UserApiRequest, ParseBodyError> {
    match query {
        Some(query) => serde_html_form::from_str(query)
            .map_err(|e| ParseBodyError::BadRequest(format!("Failed to parse query: {e}"))),
        None => Ok(UserApiRequest::default()),
    }
}

fn parse_multipart_field(part: &str) -> Option<(&str, &str)> {
    let header_end = part.find("\r\n\r\n")?;
    let headers = &part[..header_end];
    let body = &part[header_end + 4..];
    let name = extract_multipart_field_name(headers)?;
    let value = body.strip_suffix("\r\n").unwrap_or(body);
    Some((name, value))
}

fn extract_multipart_field_name(headers: &str) -> Option<&str> {
    let disposition = headers
        .split("\r\n")
        .find(|line| line.trim_start().to_ascii_lowercase().starts_with("content-disposition:"))?;

    let (_, attrs) = disposition.split_once(':')?;
    for attr in attrs.split(';').skip(1) {
        let (name, value) = attr.trim().split_once('=')?;
        if !name.trim().eq_ignore_ascii_case("name") {
            continue;
        }

        let value = value.trim();
        let unquoted_double = value.strip_prefix('"').and_then(|inner| inner.strip_suffix('"'));
        if let Some(result) = unquoted_double {
            return Some(result);
        }

        let unquoted_single = value.strip_prefix('\'').and_then(|inner| inner.strip_suffix('\''));
        if let Some(result) = unquoted_single {
            return Some(result);
        }

        return Some(value);
    }

    None
}

fn is_length_limit_error(err: &axum::Error) -> bool {
    let mut current: Option<&(dyn StdError + 'static)> = Some(err);
    while let Some(source) = current {
        if source.is::<LengthLimitError>() {
            return true;
        }
        current = source.source();
    }
    false
}

fn is_multipart_content_type(content_type: &str) -> bool {
    content_type
        .split(';')
        .next()
        .is_some_and(|media_type| media_type.trim().eq_ignore_ascii_case("multipart/form-data"))
}

fn extract_multipart_boundary(content_type: &str) -> Option<&str> {
    for param in content_type.split(';').skip(1) {
        let (name, value) = param.split_once('=')?;
        if name.trim().eq_ignore_ascii_case("boundary") {
            let trimmed = value.trim();
            let unquoted = trimmed
                .strip_prefix('"')
                .and_then(|inner| inner.strip_suffix('"'))
                .unwrap_or(trimmed);
            if !unquoted.is_empty() {
                return Some(unquoted);
            }
        }
    }
    None
}

impl UserApiRequest {
    fn set_field(&mut self, name: &str, value: &str) {
        match name {
            "username" => self.username = value.to_string(),
            "password" => self.password = value.to_string(),
            "token" => self.token = value.to_string(),
            "action" => self.action = value.to_string(),
            "series_id" => self.series_id = value.to_string(),
            "vod_id" => self.vod_id = value.to_string(),
            "stream_id" => self.stream_id = value.to_string(),
            "category_id" => self.category_id = value.to_string(),
            "limit" => self.limit = value.to_string(),
            "start" => self.start = value.to_string(),
            "end" => self.end = value.to_string(),
            "stream" => self.stream = value.to_string(),
            "duration" => self.duration = value.to_string(),
            "type" | "content_type" => self.content_type = value.to_string(),
            _ => {}
        }
    }

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
    use super::{parse_body, UserApiRequest, MAX_BODY_SIZE_BYTES};
    use axum::body::Body;
    use axum::extract::FromRequest;
    use axum::http::{Request as HttpRequest, StatusCode};

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

    #[tokio::test]
    async fn parse_body_rejects_oversized_payloads() {
        let oversized = "a".repeat(MAX_BODY_SIZE_BYTES + 1);
        let err = parse_body(Body::from(oversized), "application/x-www-form-urlencoded")
            .await
            .expect_err("oversized bodies should be rejected");

        match err {
            super::ParseBodyError::PayloadTooLarge(msg) => {
                assert!(msg.contains("body too large"), "unexpected error: {msg}");
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[tokio::test]
    async fn extractor_surfaces_oversized_body_as_bad_request() {
        let oversized = "a".repeat(MAX_BODY_SIZE_BYTES + 1);
        let request = HttpRequest::builder()
            .header("content-type", "application/x-www-form-urlencoded")
            .uri("/player_api.php")
            .body(Body::from(oversized))
            .expect("request should build");

        let response = match super::UserApiRequestQueryOrBody::from_request(request, &()).await {
            Ok(_) => panic!("oversized body should reject extractor"),
            Err(response) => response,
        };

        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test]
    async fn parse_body_detects_multipart_case_insensitively_and_preserves_field_whitespace() {
        let body = concat!(
            "--abc123\r\n",
            "Content-Disposition: form-data; name=\"username\"\r\n\r\n",
            "  alice  \r\n",
            "--abc123\r\n",
            "Content-Disposition: form-data; name=\"password\"\r\n\r\n",
            "\t secret \t\r\n",
            "--abc123--\r\n",
        );

        let parsed = parse_body(
            Body::from(body),
            "Multipart/Form-Data; charset=utf-8; boundary=\"abc123\"",
        )
        .await
        .expect("multipart body should parse");

        assert_eq!(parsed.username, "  alice  ");
        assert_eq!(parsed.password, "\t secret \t");
    }

    #[tokio::test]
    async fn parse_body_accepts_content_type_field_name_in_multipart() {
        let body = concat!(
            "--abc123\r\n",
            "Content-Disposition: form-data; name=\"content_type\"\r\n\r\n",
            "m3u_plus\r\n",
            "--abc123--\r\n",
        );

        let parsed = parse_body(
            Body::from(body),
            "multipart/form-data; boundary=abc123",
        )
        .await
        .expect("multipart body should parse");

        assert_eq!(parsed.content_type, "m3u_plus");
    }

    #[tokio::test]
    async fn parse_body_uses_content_disposition_name_not_filename() {
        let body = concat!(
            "--abc123\r\n",
            "Content-Disposition: form-data; filename=\"name=\\\"wrong\\\".txt\"; name=\"username\"\r\n\r\n",
            "alice\r\n",
            "--abc123--\r\n",
        );

        let parsed = parse_body(
            Body::from(body),
            "multipart/form-data; boundary=abc123",
        )
        .await
        .expect("multipart body should parse");

        assert_eq!(parsed.username, "alice");
    }

}
