use std::rc::Rc;
use yew::prelude::*;
use crate::model::WebConfig;
use crate::services::{AuthService, ConfigService, PlaylistService, StatusService, WebSocketService};

pub struct Services {
    pub auth: Rc<AuthService>,
    pub config: Rc<ConfigService>,
    pub status: Rc<StatusService>,
    pub playlist: Rc<PlaylistService>,
    pub websocket: Rc<WebSocketService>,
}

impl Services {
    pub fn new(config: &WebConfig) -> Self {
        let auth = Rc::new(AuthService::new());
        let config = Rc::new(ConfigService::new(config));
        let status = Rc::new(StatusService::new());
        let playlist = Rc::new(PlaylistService::new());
        let websocket = Rc::new(WebSocketService::new(Rc::clone(&status)));
        Self {
            auth,
            config,
            status,
            playlist,
            websocket
        }
    }
}

impl PartialEq for Services {
    fn eq(&self, _other: &Self) -> bool {
        true
    }
}

impl Eq for Services {}

#[derive(PartialEq, Eq, Clone)]
pub struct ServiceContext {
    services: Rc<Services>,
}

impl ServiceContext {
    pub fn new(config: &WebConfig) -> Self {
        Self {
            services: Rc::new(Services::new(config))
        }
    }

    pub fn services(&self) ->  Rc<Services> {
            self.services.clone()
    }
}

#[hook]
pub fn use_service_context() -> Rc<Services> {
    use_context::<UseStateHandle<ServiceContext>>().expect("Services context not found").services()
}