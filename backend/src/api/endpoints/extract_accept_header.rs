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

    fn from_request_parts(
        parts: &mut Parts,
        _state: &B,
    ) -> impl std::future::Future<Output = Result<Self, Self::Rejection>> + Send {
        if let Some(accept_type) = parts.headers.get(axum::http::header::ACCEPT) {
            if let Ok(val) = accept_type.to_str() {
                return std::future::ready(Ok(ExtractAcceptHeader(Some(val.to_string()))));
            }
        }
        std::future::ready(Ok(ExtractAcceptHeader(None)))
    }
}
