use wasm_bindgen::JsCast;
use web_sys::{ MouseEvent};
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
            Callback::noop()
        } else {
            let on_remove = props.on_remove.clone();
            Callback::from(move |e: MouseEvent| {
                if let Some(target) = e.target() {
                    if let Ok(element) = target.dyn_into::<web_sys::Element>() {
                        if let Some(data_label) = element.get_attribute("data-label") {
                            on_remove.emit(data_label.to_string());
                        }
                    }
                }
            })
        }
    };

    html! {
         <span class={classes!("tp__chip", props.class.clone())}>
            <span class="tp__chip__label">{ &props.label }</span>
            if props.removable {
                <span class="tp__remove" onclick={handle_remove}>{"×"}</span>
            }
        </span>
    }
}
