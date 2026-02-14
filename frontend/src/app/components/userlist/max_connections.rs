use crate::app::components::AppIcon;
use yew::prelude::*;

#[derive(Properties, Clone, PartialEq, Debug)]
pub struct MaxConnectionsProps {
    pub value: u32,
}

#[function_component]
pub fn MaxConnections(props: &MaxConnectionsProps) -> Html {
    if props.value == 0 {
        html! { <span class="tp__max-connections"><AppIcon name="Unlimited" /></span> }
    } else {
        html! { <span class="tp__max-connections">{ props.value }</span> }
    }
}
