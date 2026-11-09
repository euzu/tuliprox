use crate::services::{get_base_href, request_get, request_post};
use shared::{
    model::{StreamFailoverMembersDto, StreamFailoverSwitchRequest, StreamInfo},
    utils::concat_path_leading_slash,
};
use std::rc::Rc;

pub struct StreamsService {
    streams_path: String,
}

impl Default for StreamsService {
    fn default() -> Self { Self::new() }
}

impl StreamsService {
    pub fn new() -> Self {
        let base_href = get_base_href();
        Self { streams_path: concat_path_leading_slash(&base_href, "api/v1/streams") }
    }

    pub async fn get_streams_info(&self) -> Result<Option<Vec<Rc<StreamInfo>>>, crate::error::Error> {
        request_get::<Vec<Rc<StreamInfo>>>(&self.streams_path, None, None).await
    }

    pub async fn get_failover_members(
        &self,
        stream_id: u32,
    ) -> Result<Option<StreamFailoverMembersDto>, crate::error::Error> {
        let path = format!("{}/{}/failover-members", self.streams_path, stream_id);
        request_get::<StreamFailoverMembersDto>(&path, None, None).await
    }

    pub fn switch_failover_member_url(streams_path: &str, stream_id: u32) -> String {
        format!("{streams_path}/{stream_id}/failover-switch")
    }

    pub async fn switch_failover_member(&self, stream_id: u32, member_vid: u32) -> Result<bool, crate::error::Error> {
        let path = Self::switch_failover_member_url(&self.streams_path, stream_id);
        let req = StreamFailoverSwitchRequest { member_vid };
        interpret_switch_response(request_post::<StreamFailoverSwitchRequest, ()>(&path, req, None, None).await)
    }
}

pub(crate) fn interpret_switch_response(
    response: Result<Option<()>, crate::error::Error>,
) -> Result<bool, crate::error::Error> {
    response.map(|_| true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn switch_failover_member_url_is_expected_path() {
        assert_eq!(
            StreamsService::switch_failover_member_url("/api/v1/streams", 42),
            "/api/v1/streams/42/failover-switch"
        );
    }

    #[test]
    fn switch_failover_member_empty_200_is_treated_as_success() {
        // Ok(None) represents empty 200/204 response from request_post
        assert_eq!(interpret_switch_response(Ok(None)), Ok(true));
        assert_eq!(interpret_switch_response(Ok(Some(()))), Ok(true));
        assert!(interpret_switch_response(Err(crate::error::Error::NotFound)).is_err());
    }
}
