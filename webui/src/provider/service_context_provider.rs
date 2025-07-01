
use yew::prelude::*;
use crate::hooks::ServiceContext;
use crate::model::WebConfig;

#[derive(Properties, Clone, PartialEq)]
pub struct ServiceContextProps {
    pub children: Children,
    pub config: WebConfig
}

/// User context provider.
#[function_component]
pub fn ServiceContextProvider(props: &ServiceContextProps) -> Html {
    let service_ctx = use_state(||ServiceContext::new(&props.config));

    html! {
        <ContextProvider<UseStateHandle<ServiceContext>> context={service_ctx}>
            { for props.children.iter() }
        </ContextProvider<UseStateHandle<ServiceContext>>>
    }
}