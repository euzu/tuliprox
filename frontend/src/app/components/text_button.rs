use crate::app::components::{button_utils::prevent_default_and_stop, AppIcon};
use yew::prelude::*;

#[derive(Properties, Clone, PartialEq, Debug)]
pub struct TextButtonProps {
    pub name: String,
    #[prop_or_default]
    pub icon: String,
    pub title: String,
    #[prop_or_default]
    pub class: String,
    pub onclick: Callback<String>,
    #[prop_or_default]
    pub autofocus: bool,
    #[prop_or_default]
    pub disabled: bool,
    /// Accessible name, when the visible `title` is not enough on its
    /// own. A column of identical "Delete" buttons needs to say *what*
    /// each one deletes; the visible label stays short.
    #[prop_or(None)]
    pub aria_label: Option<String>,
    /// Optional toggle state (`true`, `false` or `mixed`); absent for ordinary actions.
    #[prop_or(None)]
    pub aria_pressed: Option<String>,
    #[prop_or(None)]
    pub hint: Option<String>,
}

#[component]
pub fn TextButton(props: &TextButtonProps) -> Html {
    let handle_click = {
        let click = props.onclick.clone();
        let name = props.name.clone();
        prevent_default_and_stop::<(), _>(move |_| {
            click.emit(name.clone());
        })
    };

    text_button_view(props, handle_click)
}

fn text_button_view(props: &TextButtonProps, handle_click: Callback<MouseEvent>) -> Html {
    html! {
        <button
            type="button"
            autofocus={props.autofocus}
            disabled={props.disabled}
            onclick={handle_click}
            aria-label={props.aria_label.clone()}
            aria-pressed={props.aria_pressed.clone()}
            title={props.hint.clone()}
            class={classes!("tp__text-button", props.class.clone())}>
         if !props.icon.is_empty() {
            <AppIcon name={props.icon.clone()}></AppIcon>
         }
         <span>{props.title.clone()}</span>
        </button>
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn text_button_touch_does_not_keep_mouse_highlights_and_preserves_keyboard_focus() {
        let styles = include_str!("../../../scss/app/components/_text_button.scss");
        let (highlight, variants) = styles.split_once(".tp__text-button {").expect("shared button styles");
        let (keyboard, mouse) =
            highlight.split_once("@media (hover: hover) and (pointer: fine)").expect("hover capability");

        assert!(keyboard.contains("&:focus-visible {\n    @content;"));
        assert!(mouse.contains("&:focus,\n    &:hover {\n      @content;"));
        assert_eq!(variants.matches("@include text-button-highlight {").count(), 5);
        assert!(!variants.contains("&:hover"));
        assert!(!variants.contains("&:focus"));
        assert!(variants.contains("font-weight: bold;"));
    }

    #[test]
    fn text_button_preserves_optional_toggle_accessibility_and_disabled_state() {
        for pressed in [None, Some("false"), Some("true"), Some("mixed")] {
            for disabled in [false, true] {
                let props = yew::props!(TextButtonProps {
                    name: "target-7",
                    title: "Target seven",
                    onclick: Callback::noop(),
                    aria_label: Some("Target seven".to_string()),
                    aria_pressed: pressed.map(str::to_string),
                    disabled,
                });
                let Html::VTag(button) = text_button_view(&props, Callback::noop()) else {
                    panic!("TextButton must render a native button");
                };
                let attributes = button.attributes.iter().collect::<HashMap<_, _>>();

                assert_eq!(button.tag(), "button");
                assert_eq!(attributes.get("type"), Some(&"button"));
                assert_eq!(attributes.get("class"), Some(&"tp__text-button"));
                assert_eq!(attributes.get("aria-label"), Some(&"Target seven"));
                assert_eq!(attributes.get("aria-pressed").copied(), pressed);
                assert_eq!(attributes.contains_key("disabled"), disabled);
            }
        }
    }
}
