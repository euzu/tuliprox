use axum::{
    extract::FromRequestParts,
    http::{request::Parts, StatusCode},
};

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct ExtractAcceptHeader(pub Option<String>);

impl<B> FromRequestParts<B> for ExtractAcceptHeader
where
    B: Send + Sync,
{
    type Rejection = (StatusCode, &'static str);

    async fn from_request_parts(parts: &mut Parts, _state: &B) -> Result<Self, Self::Rejection> {
        if let Some(accept_type) = parts.headers.get(axum::http::header::ACCEPT) {
            if let Ok(val) = accept_type.to_str() {
                return Ok(ExtractAcceptHeader(Some(val.to_string())));
            }
        }
        Ok(ExtractAcceptHeader(None))
    }
}
