use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum::http::StatusCode;
use crate::auth::Rejection;

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct AuthBearer(pub String);

impl<B> FromRequestParts<B> for AuthBearer
where
    B: Send + Sync,
{
    type Rejection = Rejection;

    async fn from_request_parts(req: &mut Parts, _: &B) -> Result<Self, Self::Rejection> {
        Self::decode_request_parts(req)
    }
}

impl AuthBearer {
    fn from_header(contents: &str) -> Self {
        Self(contents.to_string())
    }

    fn decode_request_parts(req: &mut Parts) -> Result<Self, Rejection> {
        let authorization = req
            .headers
            .get(axum::http::header::AUTHORIZATION)
            .ok_or((StatusCode::FORBIDDEN, "Authorization header is missing"))?
            .to_str()
            .map_err(|_| (StatusCode::FORBIDDEN, "Authorization header contains invalid characters"))?;

        let split = authorization.split_once(' ');
        match split {
            Some(("Bearer", contents)) => Ok(Self::from_header(contents)),
            _ => Err((StatusCode::FORBIDDEN, "`Authorization` header must be a bearer token")),
        }
    }
}
