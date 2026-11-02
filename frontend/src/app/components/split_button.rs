use crate::app::components::{popup_menu::PopupMenuPlacement, DropDownIconButton, DropDownOption, DropDownSelection};
use std::rc::Rc;
use yew::prelude::*;

/// Split button (MUI `ButtonGroup` + menu / Bootstrap split dropdown): one bordered surface with the
/// main action on the left and, separated by a divider, a caret that opens an options menu.
/// `active` marks a toggle state on the whole control.
#[derive(Properties, Clone, PartialEq)]
pub struct SplitButtonProps {
    pub name: String,
    pub title: String,
    #[prop_or_default]
    pub active: bool,
    #[prop_or_default]
    pub class: Classes,
    #[prop_or_default]
    pub aria_label: Option<String>,
    /// Accessible name of the menu caret.
    #[prop_or_default]
    pub menu_label: Option<String>,
    pub options: Rc<Vec<DropDownOption>>,
    pub onclick: Callback<String>,
    pub on_select: Callback<(String, DropDownSelection)>,
}

/// Same gap as `Select`, so both popups sit at the same distance below their control.
const SPLIT_BUTTON_POPUP_GAP_PX: f64 = 2.0;

#[component]
pub fn SplitButton(props: &SplitButtonProps) -> Html {
    let group_ref = use_node_ref();
    let on_main = {
        let onclick = props.onclick.clone();
        let name = props.name.clone();
        Callback::from(move |e: MouseEvent| {
            e.prevent_default();
            onclick.emit(name.clone());
        })
    };

    html! {
        <div ref={group_ref.clone()} class={classes!("tp__split-button", props.active.then_some("active"), props.class.clone())}
            role="group">
            <button
                type="button"
                class="tp__split-button__main"
                aria-pressed={props.active.to_string()}
                aria-label={props.aria_label.clone()}
                onclick={on_main}
            >
                { props.title.clone() }
            </button>
            <DropDownIconButton
                name={format!("{}_menu", props.name)}
                icon="ChevronDown"
                class="tp__split-button__menu"
                aria_label={props.menu_label.clone()}
                popup_placement={PopupMenuPlacement::BottomEndFlip}
                popup_gap={Some(SPLIT_BUTTON_POPUP_GAP_PX)}
                width_anchor_ref={Some(group_ref)}
                options={props.options.clone()}
                close_on_resize=true
                on_select={props.on_select.clone()}
            />
        </div>
    }
}
