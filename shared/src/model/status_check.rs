use std::collections::BTreeMap;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusCheck {
    pub status: String,
    pub version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub build_time: Option<String>,
    pub server_time: String,
    pub memory: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache: Option<String>,
    pub active_users: usize,
    pub active_user_connections: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_provider_connections: Option<BTreeMap<String, usize>>,
}