use crate::{
    app::components::{button_utils::prevent_default_and_stop, popup_menu::PopupMenu, AppIcon, IconButton},
    html_if,
};
use std::{collections::HashSet, rc::Rc};
use web_sys::MouseEvent;
use yew::{classes, component, html, use_effect_with, use_state, Callback, Html, NodeRef, Properties};
use yew_hooks::use_set;

#[derive(Debug, Clone, PartialEq)]
pub enum DropDownSelection {
    Empty,
    Single(String),
    Multi(Vec<String>),
}

#[derive(Clone, PartialEq, Debug)]
pub struct DropDownOption {
    pub(crate) id: String,
    pub(crate) label: Html,
    pub(crate) selected: bool,
}

impl DropDownOption {
    pub fn new(id: &str, label: Html, selected: bool) -> Self {
        Self { id: id.to_owned(), label, selected }
    }
}

#[derive(Properties, Clone, PartialEq, Debug)]
pub struct DropDownIconButtonProps {
    pub name: String,
    pub icon: String,
    pub on_select: Callback<(String, DropDownSelection)>,
    #[prop_or_default]
    pub class: String,
    pub options: Rc<Vec<DropDownOption>>,
    #[prop_or_default]
    pub multi_select: bool,
    #[prop_or_default]
    pub button_ref: Option<NodeRef>,
    #[prop_or_default]
    pub aria_label: Option<String>,
    #[prop_or_default]
    pub aria_required: Option<bool>,
    #[prop_or_default]
    pub aria_invalid: Option<bool>,
    #[prop_or_default]
    pub aria_describedby: Option<String>,
}

#[component]
pub fn DropDownIconButton(props: &DropDownIconButtonProps) -> Html {
    let button_ref = props.button_ref.clone().unwrap_or_default();
    let popup_anchor_ref = use_state(|| None::<web_sys::Element>);
    let popup_is_open = use_state(|| false);
    let selections = use_set(HashSet::<String>::new());

    {
        let set_selections = selections.clone();
        use_effect_with(props.options.clone(), move |options| {
            let selections = options.iter().filter(|x| x.selected).map(|x| x.id.clone()).collect::<HashSet<String>>();
            set_selections.set(selections);
        });
    }

    let handle_popup_close = {
        let set_is_open = popup_is_open.clone();
        Callback::from(move |()| {
            set_is_open.set(false);
        })
    };

    let handle_click = {
        let button_ref = button_ref.clone();
        let set_anchor_ref = popup_anchor_ref.clone();
        let set_is_open = popup_is_open.clone();
        Callback::from(move |(_name, _event): (String, MouseEvent)| {
            if let Some(button) = button_ref.cast::<web_sys::Element>() {
                set_anchor_ref.set(Some(button));
                set_is_open.set(true);
            }
        })
    };

    let handle_option_click = {
        let multi_select = props.multi_select;
        let name = props.name.clone();
        let selections = selections.clone();
        let onselect = props.on_select.clone();
        let set_is_open = popup_is_open.clone();
        Callback::from(move |(id, _event): (String, MouseEvent)| {
            let selected_options = if multi_select {
                if selections.current().contains(&id) {
                    selections.remove(&id);
                } else {
                    selections.insert(id.clone());
                }
                if selections.current().is_empty() {
                    DropDownSelection::Empty
                } else {
                    DropDownSelection::Multi(selections.current().iter().map(Clone::clone).collect::<Vec<_>>())
                }
            } else {
                // Single-select replaces the previous selection instead of toggling
                selections.set(HashSet::from([id.clone()]));
                DropDownSelection::Single(id.clone())
            };
            onselect.emit((name.clone(), selected_options));
            if !multi_select {
                set_is_open.set(false);
            }
        })
    };

    html! {
         <>
         <IconButton button_ref={button_ref} class={props.class.clone()} name={props.name.clone()} icon={props.icon.clone()} onclick={handle_click}
            role="combobox"
            aria_haspopup="listbox"
            aria_expanded={Some(*popup_is_open)}
            aria_label={props.aria_label.clone()}
            aria_required={props.aria_required}
            aria_invalid={props.aria_invalid}
            aria_describedby={props.aria_describedby.clone()} />
         <PopupMenu list_role="listbox" is_open={*popup_is_open} anchor_ref={(*popup_anchor_ref).clone()} on_close={handle_popup_close}>
            {
                for props.options.iter().map(|o| {
                    let checkbox_id = o.id.clone();
                    let checkbox_handler = handle_option_click.clone();
                    let option_click = prevent_default_and_stop::<(), _>({
                        let id = checkbox_id.clone();
                        move |event| checkbox_handler.emit((id.clone(), event))
                    });
                    html! {
                        <div role="option" aria-selected={selections.current().contains(&o.id).to_string()} class={classes!("tp__dropdown-icon-button__option", "tp__menu-item", if selections.current().contains(&o.id) {"checked"} else {"unchecked"})} onclick={option_click}>
                            {
                                html_if!(
                                    props.multi_select,
                                    {
                                        if selections.current().contains(&o.id) {
                                            <AppIcon name={"CheckMark"} />
                                        } else {
                                            <span class={"placeholder"} />
                                        }
                                })
                            }
                            <span class={"tp__dropdown-icon-button__option-item"}>{o.label.clone()}</span>
                        </div>
                }})
            }
        </PopupMenu>
        </>
    }
}
