use web_sys::MouseEvent;
use yew::{function_component, html, Callback, Html, Properties};
use crate::app::components::AppIcon;

#[derive(Properties, Clone, PartialEq, Debug)]
pub struct IconButtonProps {
    pub name: String,
    pub icon: String,
    pub onclick: Callback<String>,
}

#[function_component]
pub fn IconButton(props: &IconButtonProps) -> Html {

    let handle_click = {
        let click = props.onclick.clone();
        let name = props.name.clone();
        Callback::from(move |e: MouseEvent| {
            e.prevent_default();
            click.emit(name.clone());
        })
    };

    html! {
        <button class="tp__icon-button" onclick={handle_click}>
            <AppIcon name={props.icon.clone()}></AppIcon>
        </button>
    }
}