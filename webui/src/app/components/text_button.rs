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
    pub style: String,
    pub onclick: Callback<String>,
}

#[function_component]
pub fn TextButton(props: &TextButtonProps) -> Html {

    let handle_click = {
        let click = props.onclick.clone();
        let name = props.name.clone();
        Callback::from(move |e: MouseEvent| {
            e.prevent_default();
            click.emit(name.clone());
        })
    };

    html! {
        <button class={if props.style.is_empty() {"text-button".to_string()} else {format!("text-button button-{}", props.style)}} onclick={handle_click}>
         if !props.icon.is_empty() {
            <AppIcon name={props.icon.clone()}></AppIcon>
         }
         <span>{props.title.clone()}</span>
        </button>
    }
}