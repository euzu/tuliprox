use axum::extract::FromRequestParts;
use axum::http::HeaderMap;
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

    fn from_request_parts(
        req: &mut Parts,
        _: &B,
    ) -> impl std::future::Future<Output = Result<Self, Self::Rejection>> + Send {
        std::future::ready(Self::decode_request_parts(req))
    }
}

impl AuthBearer {
    fn from_header(contents: &str) -> Self {
        Self(contents.to_string())
    }

    pub(crate) fn from_headers(headers: &HeaderMap) -> Result<Self, Rejection> {
        let authorization = headers
            .get(axum::http::header::AUTHORIZATION)
            .ok_or((StatusCode::FORBIDDEN, "Authorization header is missing"))?
            .to_str()
            .map_err(|_| (StatusCode::FORBIDDEN, "Authorization header contains invalid characters"))?;

        let split = authorization.split_once(' ');
        match split {
            Some((scheme, contents)) if scheme.eq_ignore_ascii_case("bearer") => Ok(Self::from_header(contents)),
            _ => Err((StatusCode::FORBIDDEN, "`Authorization` header must be a bearer token")),
        }
    }

    fn decode_request_parts(req: &mut Parts) -> Result<Self, Rejection> {
        Self::from_headers(&req.headers)
    }
}
