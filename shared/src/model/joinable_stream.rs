use serde::{Deserialize, Serialize};

/// Summary of a running shared stream that a playlist user can join without a new provider connection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JoinableStreamDto {
    pub virtual_id: u32,
    pub title: String,
    pub logo: String,
    pub category: String,
    pub protocol: String,
    pub requires_free_slot: bool,
}
