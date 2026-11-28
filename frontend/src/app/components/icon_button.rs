use crate::app::components::{button_utils::prevent_default_and_stop, AppIcon};
use web_sys::MouseEvent;
use yew::{classes, component, html, Callback, Html, NodeRef, Properties};

#[derive(Properties, Clone, PartialEq, Debug)]
pub struct IconButtonProps {
    pub name: String,
    pub icon: String,
    pub onclick: Callback<(String, MouseEvent)>,
    #[prop_or_default]
    pub class: String,
    #[prop_or_default]
    pub hint: String,
    #[prop_or_default]
    pub button_ref: Option<NodeRef>,
    /// ARIA role override; `aria_required`/`aria_invalid` are only valid with e.g. `combobox`.
    #[prop_or_default]
    pub role: Option<String>,
    #[prop_or_default]
    pub aria_haspopup: Option<String>,
    #[prop_or_default]
    pub aria_expanded: Option<bool>,
    #[prop_or_default]
    pub aria_label: Option<String>,
    #[prop_or_default]
    pub aria_disabled: Option<bool>,
    #[prop_or_default]
    pub aria_required: Option<bool>,
    #[prop_or_default]
    pub aria_invalid: Option<bool>,
    #[prop_or_default]
    pub aria_describedby: Option<String>,
}

#[component]
pub fn IconButton(props: &IconButtonProps) -> Html {
    let handle_click = {
        let click = props.onclick.clone();
        let name = props.name.clone();
        let is_disabled = props.aria_disabled.unwrap_or(false);
        prevent_default_and_stop::<(), _>(move |event: MouseEvent| {
            if !is_disabled {
                click.emit((name.clone(), event));
            }
        })
    };

    html! {
            <button type="button" title={props.hint.clone()} ref={props.button_ref.clone().unwrap_or_default()} class={classes!("tp__icon-button", if props.icon == "Delete" {"tp__icon-button__remove"} else {""}, props.class.clone())} onclick={handle_click}
                role={props.role.clone()}
                aria-haspopup={props.aria_haspopup.clone()}
                aria-expanded={props.aria_expanded.map(|v| v.to_string())}
                aria-label={props.aria_label.clone()}
                aria-disabled={props.aria_disabled.map(|v| v.to_string())}
                aria-required={props.role.is_some().then(|| props.aria_required.map(|v| v.to_string())).flatten()}
                aria-invalid={props.role.is_some().then(|| props.aria_invalid.map(|v| v.to_string())).flatten()}
                aria-describedby={props.aria_describedby.clone()}>
            <AppIcon name={props.icon.clone()}></AppIcon>
        </button>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icon_button_defaults_aria_disabled_to_none_and_accepts_explicit_state() {
        let default_props = yew::props!(IconButtonProps { name: "edit", icon: "Edit", onclick: Callback::noop() });
        assert_eq!(default_props.aria_disabled, None);

        let disabled_props = yew::props!(IconButtonProps {
            name: "group_up_0",
            icon: "ArrowUp",
            onclick: Callback::noop(),
            aria_disabled: Some(true),
        });
        assert_eq!(disabled_props.aria_disabled, Some(true));
    }
}
