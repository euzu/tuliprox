use shared::utils::serialize_number_as_string;
use crate::model::{ApiProxyServerInfo, ProxyUserCredentials};
use chrono::{Duration, Local};
use serde::{Deserialize, Serialize};
use shared::model::ProxyUserStatus;
use shared::utils::CONSTANTS;

#[derive(Serialize, Deserialize, Clone)]
pub struct XtreamUserInfoResponse {
    pub username: String,
    pub password: String,
    pub message: String,
    pub auth: u16, // 0 | 1
    pub status: String, // "Active"
    #[serde(serialize_with ="serialize_number_as_string")]
    pub exp_date: i64, //1628755200,
    pub is_trial: String, // 0 | 1
    pub active_cons: String,
    #[serde(serialize_with ="serialize_number_as_string")]
    pub created_at: i64, //1623429679,
    pub max_connections: String,
    pub allowed_output_formats: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct XtreamServerInfoResponse {
    pub url: String,
    pub port: String,
    pub https_port: String,
    pub server_protocol: String, // http, https
    pub rtmp_port: String,
    pub timezone: String,
    pub timestamp_now: i64,
    pub time_now: String, //"2021-06-28 17:07:37"
    pub process: bool,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct XtreamAuthorizationResponse {
    pub user_info: XtreamUserInfoResponse,
    pub server_info: XtreamServerInfoResponse,
}

impl XtreamAuthorizationResponse {
    pub fn new(server_info: &ApiProxyServerInfo, user: &ProxyUserCredentials, active_connections: u32, access_control: bool) -> Self {
        let now = Local::now();
        let created_default = (now - Duration::days(365)).timestamp();
        let expired_default = (now + Duration::days(365)).timestamp();

        let (created_at, exp_date, is_trial, max_connections, user_status) =
            if access_control {
                let exp_date = user.exp_date.as_ref().map_or(expired_default, |d| *d);
                let is_expired = (exp_date - now.timestamp()) < 0;
                let current_status = user.status.as_ref().unwrap_or(&ProxyUserStatus::Active);
                let user_status = match current_status {
                    ProxyUserStatus::Active | ProxyUserStatus::Trial => if is_expired { &ProxyUserStatus::Expired } else { current_status },
                    _ => current_status
                };
                (user.created_at.as_ref().map_or(created_default, |d| *d),
                 exp_date,
                 user.status.as_ref().map_or("0", |s| if *s == ProxyUserStatus::Trial { "1" } else { "0" }).to_string(),
                 format!("{}", user.max_connections),
                 user_status
                )
            } else {
                (created_default,
                 expired_default,
                 "0".to_string(),
                 if user.max_connections == 0 { "1".to_string() } else { user.max_connections.to_string() },
                 &ProxyUserStatus::Active,
                )
            };

        Self {
            user_info: XtreamUserInfoResponse {
                username: user.username.clone(),
                password: user.password.clone(),
                message: server_info.message.clone(),
                auth: 1,
                status: user_status.to_string(),
                exp_date,
                is_trial,
                active_cons: active_connections.to_string(),
                created_at,
                max_connections,
                allowed_output_formats: CONSTANTS.allowed_output_formats.clone(),
            },
            server_info: XtreamServerInfoResponse {
                url: server_info.host.clone(),
                port: if server_info.protocol == "http" { server_info.port.as_ref().map_or("80", |v| v.as_str()).to_string() } else { String::from("80") },
                https_port: if server_info.protocol == "https" { server_info.port.as_ref().map_or("443", |v| v.as_str()).to_string() } else { String::from("443") },
                server_protocol: server_info.protocol.clone(),
                rtmp_port: String::new(),
                timezone: server_info.timezone.clone(),
                timestamp_now: now.timestamp(),
                time_now: now.format("%Y-%m-%d %H:%M:%S").to_string(),
                // We don't know what this field is good for, but it is in the response from XtreamCodes, so we will include it.
                process: true
            },
        }
    }
}
