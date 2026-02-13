use crate::services::{get_base_href, request_get};
use shared::model::StreamInfo;
use shared::utils::concat_path_leading_slash;
use std::rc::Rc;

pub struct StreamsService {
    streams_path: String,
}

impl Default for StreamsService {
    fn default() -> Self {
        Self::new()
    }
}

impl StreamsService {
    pub fn new() -> Self {
        let base_href = get_base_href();
        Self {
            streams_path: concat_path_leading_slash(&base_href, "api/v1/streams"),
        }
    }

    pub async fn get_streams_info(
        &self,
    ) -> Result<Option<Vec<Rc<StreamInfo>>>, crate::error::Error> {
        request_get::<Vec<Rc<StreamInfo>>>(&self.streams_path, None, None).await
    }
}
