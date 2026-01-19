use crate::utils::is_blank_optional_string;
use std::collections::BTreeMap;
use std::sync::Arc;
use serde::{Deserialize, Serialize};
use crate::model::StreamInfo;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StatusCheck {
    pub status: String,
    pub version: String,
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub build_time: Option<String>,
    pub server_time: String,
    #[serde(default, skip_serializing_if = "is_blank_optional_string")]
    pub cache: Option<String>,
    pub active_users: usize,
    pub active_user_connections: usize,
    pub active_user_streams: Vec<StreamInfo>,
    #[serde(default)]
    pub active_provider_connections: Option<BTreeMap<Arc<str>, usize>>,
}

impl Default for StatusCheck {
    fn default() -> Self {
        Self {
            status: "n/a".to_string(),
            version: "n/a".to_string(),
            build_time: None,
            server_time: "n/a".to_string(),
            cache: None,
            active_users: 0,
            active_user_connections: 0,
            active_provider_connections: None,
            active_user_streams: Vec::new(),
        }
    }
}