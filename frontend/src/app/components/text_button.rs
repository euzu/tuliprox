use crate::app::components::{button_utils::prevent_default_and_stop, AppIcon};
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
        prevent_default_and_stop::<(), _>(move |_| {
            click.emit(name.clone());
        })
    };

    html! {
        <button
            type="button"
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
