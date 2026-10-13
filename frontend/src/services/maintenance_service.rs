use crate::{
    error::Error,
    services::{get_base_href, request_delete, request_get, request_put},
};
use shared::{
    model::{MaintenanceRequestDto, MaintenanceStateDto},
    utils::concat_path_leading_slash,
};

pub struct MaintenanceService {
    path: String,
}
impl Default for MaintenanceService {
    fn default() -> Self { Self::new() }
}
impl MaintenanceService {
    pub fn new() -> Self { Self { path: concat_path_leading_slash(&get_base_href(), "api/v1/maintenance") } }
    fn input_path(&self, input: &str) -> String {
        format!("{}/{}", self.path, String::from(js_sys::encode_uri_component(input)))
    }
    pub async fn get(&self) -> Result<MaintenanceStateDto, Error> {
        request_get(&self.path, None, None).await?.ok_or(Error::DeserializeError)
    }
    /// `input` may be `*` for all inputs. Re-adding an input updates its remark.
    pub async fn add(&self, input: &str, reason: Option<String>) -> Result<MaintenanceStateDto, Error> {
        request_put(&self.input_path(input), MaintenanceRequestDto { reason }, None, None)
            .await?
            .ok_or(Error::DeserializeError)
    }
    /// `input` may be `*` for all inputs.
    pub async fn remove(&self, input: &str) -> Result<MaintenanceStateDto, Error> {
        request_delete(&self.input_path(input), None, None).await?.ok_or(Error::DeserializeError)
    }
}
