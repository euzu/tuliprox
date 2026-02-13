use crate::error::Error;
use crate::services::{get_base_href, request_delete, request_post, request_put, EventService};
use log::error;
use shared::model::ProxyUserCredentialsDto;
use shared::utils::{concat_path, concat_path_leading_slash};
use std::rc::Rc;

pub struct UserService {
    user_path: String,
    event_service: Rc<EventService>,
}

impl UserService {
    pub fn new(event_service: Rc<EventService>) -> Self {
        let base_href = get_base_href();
        Self {
            user_path: concat_path_leading_slash(&base_href, "api/v1/user"),
            event_service,
        }
    }

    pub async fn create_user(
        &self,
        target: String,
        user: ProxyUserCredentialsDto,
    ) -> Result<(), Error> {
        let path = concat_path(&self.user_path, &target);
        match request_post::<ProxyUserCredentialsDto, ()>(&path, user, None, None).await {
            Ok(_) => Ok(()),
            Err(err) => {
                error!("{err}");
                Err(err)
            }
        }
    }

    pub async fn update_user(
        &self,
        target: String,
        user: ProxyUserCredentialsDto,
    ) -> Result<(), Error> {
        let path = concat_path(&self.user_path, &target);
        self.event_service.set_config_change_message_blocked(true);
        match request_put::<ProxyUserCredentialsDto, ()>(&path, user, None, None).await {
            Ok(_) => {
                self.event_service.set_config_change_message_blocked(false);
                Ok(())
            }
            Err(err) => {
                self.event_service.set_config_change_message_blocked(false);
                error!("{err}");
                Err(err)
            }
        }
    }

    pub async fn delete_user(&self, target: String, username: String) -> Result<(), Error> {
        let path = concat_path(&concat_path(&self.user_path, &target), &username);
        self.event_service.set_config_change_message_blocked(true);
        match request_delete::<()>(&path, None, None).await {
            Ok(_) => {
                self.event_service.set_config_change_message_blocked(false);
                Ok(())
            }
            Err(err) => {
                self.event_service.set_config_change_message_blocked(false);
                error!("{err}");
                Err(err)
            }
        }
    }
}
