use yew::prelude::*;

/// Themed checkbox: a native, visually hidden `<input type="checkbox">` (keyboard and screen
/// reader behaviour stay native) with a styled box. Clicks do not bubble, so a checkbox inside a
/// clickable table row or card toggles only itself.
#[derive(Properties, Clone, PartialEq)]
pub struct CheckboxProps {
    #[prop_or_default]
    pub checked: bool,
    #[prop_or_default]
    pub disabled: bool,
    #[prop_or_default]
    pub aria_label: Option<String>,
    #[prop_or_default]
    pub class: Classes,
    pub onchange: Callback<Event>,
}

#[component]
pub fn Checkbox(props: &CheckboxProps) -> Html {
    let stop_click = Callback::from(|e: MouseEvent| e.stop_propagation());
    html! {
        <label class={classes!("tp__checkbox", props.checked.then_some("tp__checkbox--checked"), props.class.clone())}
            onclick={stop_click}>
            <input
                type="checkbox"
                class="tp__checkbox__input"
                checked={props.checked}
                disabled={props.disabled}
                aria-label={props.aria_label.clone()}
                onchange={props.onchange.clone()}
            />
            <span class="tp__checkbox__box" aria-hidden="true"></span>
        </label>
    }
}
