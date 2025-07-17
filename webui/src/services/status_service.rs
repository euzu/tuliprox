use std::rc::Rc;
use crate::services::request_get;
use shared::model::StatusCheck;

const STATUS_PATH: &str = "/api/v1/status";


pub struct StatusService {}

impl Default for StatusService {
    fn default() -> Self {
        Self::new()
    }
}

impl StatusService {
    pub fn new() -> Self {
        Self {}
    }

    pub async fn get_server_status(&self) -> Result<Rc<StatusCheck>, crate::error::Error> {
        request_get::<Rc<StatusCheck>>(STATUS_PATH).await
    }
}