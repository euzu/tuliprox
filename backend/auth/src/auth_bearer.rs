use axum::{
    extract::FromRequestParts,
    http::{header::AUTHORIZATION, request::Parts, HeaderMap},
};
use tuliprox_core::model::{AuthRejection, AuthScheme};

use crate::Rejection;

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

    pub fn from_headers(headers: &HeaderMap) -> Result<Self, Rejection> {
        let authorization = headers
            .get(AUTHORIZATION)
            .ok_or(AuthRejection::MissingHeader(AuthScheme::Bearer))?
            .to_str()
            .map_err(|_| AuthRejection::MalformedHeader(AuthScheme::Bearer))?;

        match authorization.split_once(' ') {
            Some((scheme, contents)) if scheme.eq_ignore_ascii_case("bearer") => Ok(Self::from_header(contents)),
            _ => Err(AuthRejection::WrongScheme(AuthScheme::Bearer)),
        }
    }

    fn decode_request_parts(req: &mut Parts) -> Result<Self, Rejection> {
        Self::from_headers(&req.headers)
    }
}
