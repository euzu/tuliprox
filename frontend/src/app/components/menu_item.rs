use crate::{app::components::AppIcon, html_if};
use web_sys::MouseEvent;
use yew::prelude::*;

#[derive(Properties, Clone, PartialEq, Debug)]
pub struct MenuItemProps {
    pub name: String,
    pub label: String,
    #[prop_or_default]
    pub icon: String,
    #[prop_or_default]
    pub class: String,
    #[prop_or_default]
    pub onclick: Callback<(String, MouseEvent)>,
}

#[component]
pub fn MenuItem(props: &MenuItemProps) -> Html {
    let handle_click = {
        let click = props.onclick.clone();
        let name = props.name.clone();
        Callback::from(move |e: MouseEvent| {
            e.prevent_default();
            e.stop_propagation();
            click.emit((name.clone(), e));
        })
    };

    let is_active = props.class.split_whitespace().any(|c| c == "active");

    html! {
        <div
            class={classes!("tp__menu-item", props.class.clone())}
            onclick={ handle_click }
            role="button"
            tabindex="0"
            aria-current={if is_active { Some("page") } else { None }}
        >
            {html_if!(!props.icon.is_empty(),
                {<AppIcon name={props.icon.clone()}></AppIcon> }
            )}
            <label>{props.label.clone()}</label>
        </div>
    }
}
