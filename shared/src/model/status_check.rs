use std::collections::BTreeMap;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
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

impl Default for StatusCheck {
    fn default() -> Self {
        Self {
            status: "n/a".to_string(),
            version: "n/a".to_string(),
            build_time: None,
            server_time: "n/a".to_string(),
            memory: "n/a".to_string(),
            cache: None,
            active_users: 0,
            active_user_connections: 0,
            active_provider_connections: None,
        }
    }
}