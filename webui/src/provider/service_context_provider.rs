
use yew::prelude::*;
use crate::config::Config;
use crate::hooks::ServiceContext;

#[derive(Properties, Clone, PartialEq)]
pub struct ServiceContextProps {
    pub children: Children,
    pub config: Config
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