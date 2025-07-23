use shared::model::{ProxyUserStatus};
use yew::prelude::*;
use crate::app::components::Chip;

fn convert_status_to_chip_style(status: &ProxyUserStatus) -> String {
     format!("tp__user-status tp__user-status__{}", status.to_string().to_lowercase())
}

#[derive(Properties, Clone, PartialEq, Debug)]
pub struct UserStatusProps {
    pub status: Option<ProxyUserStatus>,
}

#[function_component]
pub fn UserStatus(props: &UserStatusProps) -> Html {
    match props.status.as_ref() {
        Some(status) => html! {
            <Chip class={ convert_status_to_chip_style(status) }
                  label={status.to_string()} />
        },
        None => html! {},
    }
}