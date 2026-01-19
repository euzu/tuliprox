use web_sys::MouseEvent;
use yew::{function_component, html, Callback, Html, Properties};
use crate::app::components::AppIcon;

#[derive(Properties, Clone, PartialEq, Debug)]
pub struct TextButtonProps {
    pub name: String,
    #[prop_or_default]
    pub icon: String,
    pub title: String,
    #[prop_or_default]
    pub class: String,
    pub onclick: Callback<String>,
    #[prop_or_default]
    pub autofocus: bool,
}

#[function_component]
pub fn TextButton(props: &TextButtonProps) -> Html {

    let handle_click = {
        let click = props.onclick.clone();
        let name = props.name.clone();
        Callback::from(move |e: MouseEvent| {
            e.prevent_default();
            e.stop_propagation();
            click.emit(name.clone());
        })
    };

    html! {
        <button
            autofocus={props.autofocus}
            onclick={handle_click}
            class={if props.class.is_empty() {"tp__text-button".to_string()} else {format!("tp__text-button tp__button-{}", props.class)}}>
         if !props.icon.is_empty() {
            <AppIcon name={props.icon.clone()}></AppIcon>
         }
         <span>{props.title.clone()}</span>
        </button>
    }
}