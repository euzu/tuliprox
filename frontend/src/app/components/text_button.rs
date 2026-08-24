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
    /// Accessible name, when the visible `title` is not enough on its
    /// own. A column of identical "Delete" buttons needs to say *what*
    /// each one deletes; the visible label stays short.
    #[prop_or(None)]
    pub aria_label: Option<String>,
    #[prop_or(None)]
    pub hint: Option<String>,
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
            aria-label={props.aria_label.clone()}
            title={props.hint.clone()}
            class={classes!("tp__text-button", props.class.clone())}>
         if !props.icon.is_empty() {
            <AppIcon name={props.icon.clone()}></AppIcon>
         }
         <span>{props.title.clone()}</span>
        </button>
    }
}
