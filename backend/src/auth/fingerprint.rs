use std::net::SocketAddr;
use axum::extract::{ConnectInfo, FromRequestParts};
use axum::http::request::Parts;
use axum::http::StatusCode;
use crate::auth::Rejection;

const MAX_HEADER_LENGTH: usize = 512;

fn validate_header(value: &str) -> Option<String> {
    // TODO i think this is unnecessary because axum validates the headers ?
    if value.len() <= MAX_HEADER_LENGTH && !value.contains('\0') {
       Some(value.to_string())
    } else {
        None
    }
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct Fingerprint(pub String, pub String);


impl<B> FromRequestParts<B> for Fingerprint
where
    B: Send + Sync,
{
    type Rejection = Rejection;

    async fn from_request_parts(req: &mut Parts, state: &B) -> Result<Self, Self::Rejection> {
        Self::decode_request_parts(req, state).await
    }
}

impl Fingerprint {

    async fn decode_request_parts<B>(req: &mut Parts, state: &B) -> Result<Self, Rejection>
    where
        B: Send + Sync,
    {
        let ConnectInfo(addr) = ConnectInfo::<SocketAddr>::from_request_parts(req, state)
            .await
            .map_err(|_| (StatusCode::BAD_REQUEST, "IP-Addr is missing"))?;

        let mut user_agent = None;
        let mut forwarded_for = None;
        let mut real_ip = None;
        for header in &req.headers {
            if  header.0.as_str().eq_ignore_ascii_case(axum::http::header::USER_AGENT.as_str()) {
                if let Ok(val) = header.1.to_str() {
                    user_agent = validate_header(val);
                }
            } else if  header.0.as_str().eq_ignore_ascii_case("x-forwarded-for") {
                if let Ok(val) = header.1.to_str() {
                    forwarded_for = validate_header(val);
                }
            } else if  header.0.as_str().eq_ignore_ascii_case("x-real-ip") {
                if let Ok(val) = header.1.to_str() {
                    real_ip = validate_header(val);
                }
            }
        }

        let ua = user_agent.unwrap_or_else(String::new);
        let key = match real_ip.or(forwarded_for) {
            Some(xff) => format!("{}{xff}{ua}", addr.ip()),
            None => format!("{}{ua}", addr.ip()),
        };

        Ok(Fingerprint(key, addr.to_string()))
    }
}