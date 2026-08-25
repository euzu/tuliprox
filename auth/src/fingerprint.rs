use crate::Rejection;
use axum::{
    extract::{ConnectInfo, FromRequestParts},
    http::{request::Parts, StatusCode},
};
use std::net::SocketAddr;

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
pub struct Fingerprint {
    pub key: String,
    pub client_ip: String,
    pub addr: SocketAddr,
}

impl Fingerprint {
    pub fn new(key: String, client_ip: String, addr: SocketAddr) -> Self { Self { key, client_ip, addr } }
}

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
            if header.0.as_str().eq_ignore_ascii_case(axum::http::header::USER_AGENT.as_str()) {
                if let Ok(val) = header.1.to_str() {
                    user_agent = validate_header(val);
                }
            } else if header.0.as_str().eq_ignore_ascii_case("x-forwarded-for") {
                if let Ok(val) = header.1.to_str() {
                    forwarded_for = validate_header(val);
                }
            } else if header.0.as_str().eq_ignore_ascii_case("x-real-ip") {
                if let Ok(val) = header.1.to_str() {
                    real_ip = validate_header(val);
                }
            }
        }

        let client_ip = real_ip
            // X-Forwarded-For may be a comma-separated chain; the first entry is the client
            .or_else(|| forwarded_for.and_then(|list| list.split(',').next().map(|ip| ip.trim().to_string())))
            .unwrap_or_else(|| addr.ip().to_string());

        let ua = user_agent.unwrap_or_else(String::new);
        let key = format!("{client_ip}|{ua}");

        // debug!("{key}, {client_ip}, {addr}");

        Ok(Fingerprint::new(key, client_ip, addr))
    }
}
