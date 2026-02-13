use crate::services::{get_base_href, request_get};
use shared::model::StatusCheck;
use shared::utils::concat_path_leading_slash;
use std::rc::Rc;

pub struct StatusService {
    status_path: String,
}

impl Default for StatusService {
    fn default() -> Self {
        Self::new()
    }
}

impl StatusService {
    pub fn new() -> Self {
        let base_href = get_base_href();
        Self {
            status_path: concat_path_leading_slash(&base_href, "api/v1/status"),
        }
    }

    pub async fn get_server_status(&self) -> Result<Option<Rc<StatusCheck>>, crate::error::Error> {
        request_get::<Rc<StatusCheck>>(&self.status_path, None, None).await
    }
}
