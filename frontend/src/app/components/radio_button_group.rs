use crate::app::components::TextButton;
use std::rc::Rc;
use yew::prelude::*;

#[derive(Properties, PartialEq, Clone)]
pub struct RadioButtonGroupProps {
    pub options: Rc<Vec<String>>,
    pub selected: Rc<Vec<String>>,
    pub on_select: Callback<Rc<Vec<String>>>,
    #[prop_or(false)]
    pub multi_select: bool,
    #[prop_or(false)]
    pub none_allowed: bool,
}

#[function_component]
pub fn RadioButtonGroup(props: &RadioButtonGroupProps) -> Html {
    let selections = use_state(|| props.selected.clone());

    {
        let set_selections = selections.clone();
        use_effect_with(props.selected.clone(), move |selected| {
            set_selections.set(selected.clone());
        })
    }

    let on_click = {
        let on_change = props.on_select.clone();
        let set_selections = selections.clone();
        let multiselect = props.multi_select;
        let none_allowed = props.none_allowed;

        Callback::from(move |value: String| {
            let mut sel_list = (*set_selections).as_ref().clone();
            let is_selected = sel_list.contains(&value);

            if multiselect {
                // Multi-Select logic
                if is_selected {
                    // If none_allowed is false, the last selections remains
                    if none_allowed || sel_list.len() > 1 {
                        sel_list.retain(|v| v != &value);
                    }
                } else {
                    sel_list.push(value.clone());
                }
            } else {
                // Single-Select logic
                if is_selected {
                    // If none_allowed active, clear allowed
                    if none_allowed {
                        sel_list.clear();
                    }
                } else {
                    sel_list.clear();
                    sel_list.push(value.clone());
                }
            }
            let list = Rc::new(sel_list);
            set_selections.set(list.clone());
            on_change.emit(list);
        })
    };

    html! {
        <div class="tp__radio-button-group">
            { for props.options.iter().map(|option| {
                let is_selected = (*selections).contains(option);
                let class = if is_selected { "primary" } else { "" };
                let onclick = on_click.clone();
                html! {
                    <TextButton {onclick} class={class} name={ option.clone() } title={ option.clone() }></TextButton>
                }
            }) }
        </div>
    }
}
