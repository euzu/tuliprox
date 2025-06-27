use std::rc::Rc;
use shared::model::{ConfigDto};
use crate::config::Config;
use crate::services::{request_get};

const CONFIG_PATH: &str = "/api/v1/config";


pub struct ConfigService {
    pub ui_config: Rc<Config>,
}

impl ConfigService {
    pub fn new(config: &Config) -> Self {
        Self {
            ui_config: Rc::new(config.clone()),
        }
    }

    pub async fn get_server_config(&self) -> Result<ConfigDto, crate::error::Error> {
        request_get::<ConfigDto>(CONFIG_PATH).await
    }

}