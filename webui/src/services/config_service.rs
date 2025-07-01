use crate::model::WebConfig;
use crate::services::request_get;
use shared::model::{AppConfigDto, IpCheckDto};
use std::cell::RefCell;
use std::future::Future;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use log::error;
use futures_signals::signal::Mutable;
use futures_signals::signal::SignalExt;


const CONFIG_PATH: &str = "/api/v1/config";
const IP_CHECK_PATH: &str = "/api/v1/ipinfo";

pub struct ConfigService {
    pub ui_config: Rc<WebConfig>,
    pub server_config: RefCell<Option<Rc<AppConfigDto>>>,
    config_channel: Mutable<Option<Rc<AppConfigDto>>>,
    is_fetching: AtomicBool,
}

impl ConfigService {
    pub fn new(config: &WebConfig) -> Self {
        Self {
            ui_config: Rc::new(config.clone()),
            server_config: RefCell::new(None),
            config_channel: Mutable::new(None),
            is_fetching: AtomicBool::new(false),
        }
    }

    pub async fn config_subscribe<F, U>(&self, callback: &mut F)
    where
        U: Future<Output=()>,
        F: FnMut(Option<Rc<AppConfigDto>>) -> U,
    {
        let fut = self.config_channel.signal_cloned().for_each(callback);
        fut.await
    }

    pub async fn get_server_config(&self) -> Option<Rc<AppConfigDto>> {
        self.fetch_server_config().await;
        self.server_config.borrow().clone()
    }

    async fn fetch_server_config(&self) {
        if self.is_fetching.swap(true, Ordering::SeqCst) {
            return;
        }
        let result = match request_get::<AppConfigDto>(CONFIG_PATH).await {
            Ok(cfg) => Some(Rc::new(cfg)),
            Err(err) => {
                error!("{err}");
                None
            }
        };
        self.server_config.replace(result.clone());
        self.config_channel.set(result);
        self.is_fetching.store(false, Ordering::SeqCst);
    }

    pub async fn get_ip_info(&self) -> Option<IpCheckDto> {
        match request_get::<IpCheckDto>(IP_CHECK_PATH).await {
            Ok(cfg) => Some(cfg),
            Err(err) => {
                error!("{err}");
                None
            }
        }
    }
}
