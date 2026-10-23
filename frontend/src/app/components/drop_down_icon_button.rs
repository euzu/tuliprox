use crate::{
    app::components::{
        button_utils::prevent_default_and_stop,
        popup_menu::{PopupMenu, PopupMenuPlacement, PopupMenuWidth},
        select_helpers::html_text,
        AppIcon, IconButton, Search,
    },
    html_if,
};
use shared::model::SearchRequest;
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
    pub fn new(id: &str, label: Html, selected: bool) -> Self { Self { id: id.to_owned(), label, selected } }
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
    /// Shows a filter field above the options; options match by the text of their label.
    #[prop_or_default]
    pub searchable: bool,
    #[prop_or_default]
    pub button_ref: Option<NodeRef>,
    #[prop_or_default]
    pub popup_width: PopupMenuWidth,
    #[prop_or_default]
    pub popup_placement: PopupMenuPlacement,
    #[prop_or_default]
    pub width_anchor_ref: Option<NodeRef>,
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
    let filter = use_state(|| SearchRequest::Clear);

    {
        let set_selections = selections.clone();
        use_effect_with(props.options.clone(), move |options| {
            let selections = options.iter().filter(|x| x.selected).map(|x| x.id.clone()).collect::<HashSet<String>>();
            set_selections.set(selections);
        });
    }

    let handle_popup_close = {
        let set_is_open = popup_is_open.clone();
        let button_ref = button_ref.clone();
        Callback::from(move |()| {
            set_is_open.set(false);
            if let Some(button) = button_ref.cast::<web_sys::HtmlElement>() {
                let _ = button.focus();
            }
        })
    };

    let handle_click = {
        let button_ref = button_ref.clone();
        let set_anchor_ref = popup_anchor_ref.clone();
        let set_is_open = popup_is_open.clone();
        let filter = filter.clone();
        Callback::from(move |(_name, _event): (String, MouseEvent)| {
            if let Some(button) = button_ref.cast::<web_sys::Element>() {
                filter.set(SearchRequest::Clear);
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
        let button_ref = button_ref.clone();
        Callback::from(move |id: String| {
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
                if let Some(button) = button_ref.cast::<web_sys::HtmlElement>() {
                    let _ = button.focus();
                }
            }
        })
    };

    let matches_label = filter.name_matcher();
    let visible = props
        .options
        .iter()
        .filter(|option| !props.searchable || matches_label(&html_text(&option.label)))
        .collect::<Vec<_>>();
    // The filter field only exists while the popup is open; a closed popup stays free of a second search field.
    // Enter picks the first option that matches the submitted search.
    let handle_submit = {
        let options = props.options.clone();
        let handle_option_click = handle_option_click.clone();
        Callback::from(move |request: SearchRequest| {
            let matches_label = request.name_matcher();
            if let Some(option) = options.iter().find(|option| matches_label(&html_text(&option.label))) {
                handle_option_click.emit(option.id.clone());
            }
        })
    };
    let header = (props.searchable && *popup_is_open).then(|| {
        let filter = filter.clone();
        html! {
            <Search min_length={1} on_submit={Some(handle_submit)}
                onsearch={Some(Callback::from(move |request: SearchRequest| filter.set(request)))}/>
        }
    });

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
         <PopupMenu list_role="listbox" is_open={*popup_is_open} anchor_ref={(*popup_anchor_ref).clone()}
            width={props.popup_width} placement={props.popup_placement}
            width_anchor_ref={props.width_anchor_ref.clone()} on_close={handle_popup_close} header={header}>
            {
                for visible.into_iter().map(|o| {
                    let checkbox_id = o.id.clone();
                    let checkbox_handler = handle_option_click.clone();
                    let option_click = prevent_default_and_stop::<(), _>({
                        let id = checkbox_id.clone();
                        move |_event| checkbox_handler.emit(id.clone())
                    });
                    html! {
                        <button type="button" role="option" aria-selected={selections.current().contains(&o.id).to_string()} class={classes!("tp__dropdown-icon-button__option", "tp__menu-item", if selections.current().contains(&o.id) {"checked"} else {"unchecked"})} onclick={option_click}>
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
                        </button>
                }})
            }
        </PopupMenu>
        </>
    }
}
