use web_sys::MouseEvent;
use yew::prelude::*;
use crate::app::components::AppIcon;

#[derive(Properties, Clone, PartialEq, Debug)]
pub struct MenuItemProps {
    pub name: String,
    pub label: String,
    pub icon: String,
    #[prop_or_default]
    pub style: String,
    #[prop_or_default]
    pub onclick: Callback<String>,
}

#[function_component]
pub fn MenuItem(props: &MenuItemProps) -> Html {

    let handle_click = {
        let click = props.onclick.clone();
        let name = props.name.clone();
        Callback::from(move |e: MouseEvent| {
            e.prevent_default();
            click.emit(name.clone());
        })
    };

    html! {
        <div class={classes!("tp__menu-item", props.style.to_string())} onclick={ handle_click }>
            <AppIcon name={props.icon.clone()}></AppIcon>
            <label>{props.label.clone()}</label>
        </div>
    }
}