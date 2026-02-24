use crate::app::components::AppIcon;
use web_sys::MouseEvent;
use yew::prelude::*;

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
    #[prop_or_default]
    pub disabled: bool,
}

#[component]
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
            disabled={props.disabled}
            onclick={handle_click}
            class={classes!("tp__text-button", props.class.clone())}>
         if !props.icon.is_empty() {
            <AppIcon name={props.icon.clone()}></AppIcon>
         }
         <span>{props.title.clone()}</span>
        </button>
    }
}
