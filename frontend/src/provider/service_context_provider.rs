use crate::{hooks::ServiceContext, model::WebConfig, services::FlagsService};
use yew::prelude::*;

#[derive(Properties, Clone, PartialEq)]
pub struct ServiceContextProps {
    pub children: Children,
    pub config: WebConfig,
}

#[component]
pub fn ServiceContextProvider(props: &ServiceContextProps) -> Html {
    let config = props.config.clone();
    let service_ctx = use_state(move || ServiceContext::new(&config, FlagsService::new()));

    html! {
        <ContextProvider<UseStateHandle<ServiceContext>> context={service_ctx}>
            { for props.children.iter() }
        </ContextProvider<UseStateHandle<ServiceContext>>>
    }
}
