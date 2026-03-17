use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum::http::StatusCode;
use base64::Engine;
use base64::engine::general_purpose;
use crate::auth::Rejection;

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct AuthBasic(pub (String, String));

impl<B> FromRequestParts<B> for AuthBasic
where
    B: Send + Sync,
{
    type Rejection = Rejection;

    async fn from_request_parts(req: &mut Parts, _: &B) -> Result<Self, Self::Rejection> {
        Self::decode_request_parts(req)
    }
}

impl AuthBasic {
    fn from_header(contents: (String, String)) -> Self {
        Self(contents)
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
            Some(("Basic", contents)) => {
                let decoded = decode(contents)?;
                Ok(Self::from_header(decoded))
            },
            _ => Err((StatusCode::FORBIDDEN, "`Authorization` header must be a basic auth")),
        }
    }
}

/// Decodes the two parts of basic auth using the colon
fn decode(input: &str) -> Result<(String, String), Rejection> {
    // Decode from base64 into a string
    let decoded = general_purpose::STANDARD.decode(input).map_err(|_|  (StatusCode::FORBIDDEN, "Authorization header contains invalid characters"))?;
    let decoded = String::from_utf8(decoded).map_err(|_| (StatusCode::FORBIDDEN, "Authorization header contains invalid characters"))?;


    // Return depending on if password is present
    if let Some((username, password)) = decoded.split_once(':') {
        Ok((username.trim().to_string(), password.trim().to_string()))
    } else {
        Err((StatusCode::FORBIDDEN, "Authorization header contains no password"))
    }
}