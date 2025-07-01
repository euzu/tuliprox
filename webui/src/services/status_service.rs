use shared::model::StatusCheck;
use crate::services::{request_get};

const STATUS_PATH: &str = "/api/v1/status";


pub struct StatusService {
}

impl StatusService {
    pub fn new() -> Self {
        Self {
        }
    }

    pub async fn get_server_status(&self) -> Result<StatusCheck, crate::error::Error> {
        request_get::<StatusCheck>(STATUS_PATH).await
    }

}