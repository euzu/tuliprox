use crate::app::components::AppIcon;
use yew::{classes, function_component, html, Callback, Html, Properties};

pub fn convert_bool_to_chip_style(value: bool) -> Option<String> {
    Option::from((if value { "active" } else { "inactive" }).to_string())
}

#[derive(Properties, Clone, PartialEq)]
pub struct ChipProps {
    pub label: String,
    #[prop_or(None)]
    pub class: Option<String>,
    #[prop_or(false)]
    pub removable: bool,
    #[prop_or_else(Callback::noop)]
    pub on_remove: Callback<String>,
}

#[function_component]
pub fn Chip(props: &ChipProps) -> Html {
    let handle_remove = {
        if props.removable {
            let on_remove = props.on_remove.clone();
            Callback::from(move |label: String| on_remove.emit(label))
        } else {
            Callback::noop()
        }
    };

    let remove_button = if props.removable {
        let remove = handle_remove.clone();
        let label = props.label.clone();
        let on_remove = Callback::from(move |_| remove.emit(label.clone()));
        html! {
            <span class="tp__chip__remove" onclick={on_remove}>
               <AppIcon name="Delete"/>
            </span>
        }
    } else {
        html! {}
    };

    html! {
         <span class={classes!("tp__chip", props.class.clone())}>
            <span class="tp__chip__label">{ &props.label }</span>
           { remove_button }
        </span>
    }
}
