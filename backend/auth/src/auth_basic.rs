use axum::{
    extract::FromRequestParts,
    http::{header::AUTHORIZATION, request::Parts},
};
use base64::{engine::general_purpose, Engine};
use tuliprox_core::model::{AuthRejection, AuthScheme};

use crate::Rejection;

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct AuthBasic(pub (String, String));

impl<B> FromRequestParts<B> for AuthBasic
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

impl AuthBasic {
    fn from_header(contents: (String, String)) -> Self {
        Self(contents)
    }

    fn decode_request_parts(req: &mut Parts) -> Result<Self, Rejection> {
        let authorization = req
            .headers
            .get(AUTHORIZATION)
            .ok_or(AuthRejection::MissingHeader(AuthScheme::Basic))?
            .to_str()
            .map_err(|_| AuthRejection::MalformedHeader(AuthScheme::Basic))?;

        match authorization.split_once(' ') {
            Some((scheme, contents)) if scheme.eq_ignore_ascii_case("Basic") => {
                Ok(Self::from_header(decode(contents)?))
            }
            _ => Err(AuthRejection::WrongScheme(AuthScheme::Basic)),
        }
    }
}

/// Decodes the two parts of basic auth using the colon
fn decode(input: &str) -> Result<(String, String), Rejection> {
    // Decode from base64 into a string
    let decoded =
        general_purpose::STANDARD.decode(input).map_err(|_| AuthRejection::MalformedHeader(AuthScheme::Basic))?;
    let decoded = String::from_utf8(decoded).map_err(|_| AuthRejection::MalformedHeader(AuthScheme::Basic))?;

    // Return depending on if password is present
    decoded
        .split_once(':')
        .map(|(username, password)| (username.trim().to_string(), password.trim().to_string()))
        .ok_or(AuthRejection::MissingBasicPassword)
}
