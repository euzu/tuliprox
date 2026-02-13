use crate::app::components::{DropDownIconButton, DropDownOption, DropDownSelection};
use std::rc::Rc;
use yew::prelude::*;

fn map_selection(o: &DropDownOption) -> Html {
    html! {<span>{o.label.clone()}</span>}
}

#[derive(Properties, Clone, PartialEq, Debug)]
pub struct SelectProps {
    pub name: String,
    #[prop_or_default]
    pub icon: Option<String>,
    pub on_select: Callback<(String, DropDownSelection)>,
    #[prop_or_default]
    pub class: String,
    pub options: Rc<Vec<DropDownOption>>,
    #[prop_or_default]
    pub multi_select: bool,
}

#[function_component]
pub fn Select(props: &SelectProps) -> Html {
    let button_ref = use_node_ref();

    let selected_options = use_state(Vec::new);
    {
        let set_selected_options = selected_options.clone();
        use_effect_with(
            props.options.clone(),
            move |options: &Rc<Vec<DropDownOption>>| {
                let selections = options
                    .iter()
                    .filter(|o| o.selected)
                    .map(map_selection)
                    .collect::<Vec<Html>>();
                set_selected_options.set(selections);
            },
        );
    }

    let handle_click_button = {
        let button_ref = button_ref.clone();
        Callback::from(move |event: MouseEvent| {
            if let Some(target) = event.target_dyn_into::<web_sys::Element>() {
                if target.class_name().contains("tp__select-wrapper") {
                    if let Some(button) = button_ref.cast::<web_sys::HtmlElement>() {
                        button.click();
                    }
                }
            }
        })
    };

    html! {
        <div class={classes!("tp__select", props.class.clone())}>
            <div class="tp__select-wrapper" onclick={handle_click_button}>
                <div class="tp__select__selected">
                    {(*selected_options).clone()}
                </div>
                <DropDownIconButton
                     button_ref={button_ref}
                     multi_select={props.multi_select}
                     options={props.options.clone()}
                     name={props.name.clone()}
                     icon={props.icon.as_ref().map_or_else(|| "Popup".to_owned(), |i|i.to_string())}
                     on_select={props.on_select.clone()} />
            </div>
        </div>
    }
}
