use crate::app::components::{
    popup_menu::{PopupMenuPlacement, PopupMenuWidth},
    DropDownIconButton, DropDownOption, DropDownSelection,
};
use std::rc::Rc;
use yew::prelude::*;

fn map_selection(o: &DropDownOption) -> Html {
    html! {<span>{o.label.clone()}</span>}
}

/// A select's list hugs the field instead of floating at the default menu distance.
const SELECT_POPUP_GAP_PX: f64 = 2.0;

#[derive(Properties, Clone, PartialEq, Debug)]
pub struct SelectProps {
    pub name: String,
    #[prop_or_default]
    pub icon: Option<String>,
    pub on_select: Callback<(String, DropDownSelection)>,
    #[prop_or_default]
    pub class: String,
    #[prop_or_default]
    pub popup_width: PopupMenuWidth,
    /// Selects right-align their list with the field so it lines up with the input instead of
    /// hanging off the icon button on the field's right end, and flip it above the field when the
    /// space below is too small.
    #[prop_or(PopupMenuPlacement::BottomEndFlip)]
    pub popup_placement: PopupMenuPlacement,
    pub options: Rc<Vec<DropDownOption>>,
    #[prop_or_default]
    pub multi_select: bool,
    #[prop_or_default]
    pub required: bool,
    #[prop_or_default]
    pub error: Option<String>,
    #[prop_or_default]
    pub aria_describedby: Option<String>,
}

#[component]
pub fn Select(props: &SelectProps) -> Html {
    let button_ref = use_node_ref();
    let field_ref = use_node_ref();
    let error_id = props.error.as_ref().map(|_| format!("{}-select-error", props.name));
    let aria_describedby = match (&props.aria_describedby, &error_id) {
        (Some(description), Some(error)) => Some(format!("{description} {error}")),
        (Some(description), None) => Some(description.clone()),
        (None, Some(error)) => Some(error.clone()),
        (None, None) => None,
    };

    let selected_options = use_state(Vec::new);
    {
        let set_selected_options = selected_options.clone();
        use_effect_with(props.options.clone(), move |options: &Rc<Vec<DropDownOption>>| {
            let selections = options.iter().filter(|o| o.selected).map(map_selection).collect::<Vec<Html>>();
            set_selected_options.set(selections);
        });
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
        <div class={classes!(
            "tp__select",
            props.required.then_some("tp__input--required"),
            props.error.as_ref().map(|_| "tp__input--error"),
            props.class.clone())}>
            <div class="tp__select-wrapper" ref={field_ref.clone()} onclick={handle_click_button}>
                <div class="tp__select__selected">
                    {(*selected_options).clone()}
                </div>
                <DropDownIconButton
                     button_ref={button_ref}
                     popup_width={props.popup_width}
                     popup_placement={props.popup_placement}
                     popup_gap={Some(SELECT_POPUP_GAP_PX)}
                     popup_class={classes!("tp__popup-menu--select")}
                     width_anchor_ref={Some(field_ref)}
                     multi_select={props.multi_select}
                     options={props.options.clone()}
                     name={props.name.clone()}
                     icon={props.icon.as_ref().map_or_else(|| "Popup".to_owned(), std::clone::Clone::clone)}
                     aria_label={props.name.clone()}
                     aria_required={props.required.then_some(true)}
                     aria_invalid={props.error.as_ref().map(|_| true)}
                     aria_describedby={aria_describedby}
                     on_select={props.on_select.clone()} />
            </div>
            { props.error.as_ref().map_or_else(Html::default, |error| html! {
                <span id={error_id.clone()} class="tp__input-error" role="alert">{ error.clone() }</span>
            }) }
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn playlist_update_dropdown_select_width_is_opt_in_and_passed_to_the_popup() {
        let props = yew::props!(SelectProps {
            name: "default-select",
            options: Rc::new(Vec::new()),
            on_select: Callback::noop(),
        });
        assert_eq!(props.popup_width, PopupMenuWidth::Content);
        assert_eq!(props.popup_placement, PopupMenuPlacement::BottomEndFlip);
        let source = include_str!("select.rs").split("#[cfg(test)]").next().unwrap();
        let dropdown = include_str!("drop_down_icon_button.rs");
        assert!(source.contains("ref={field_ref.clone()}"));
        assert!(source.contains("popup_width={props.popup_width}"));
        assert!(source.contains("popup_placement={props.popup_placement}"));
        assert!(source.contains("width_anchor_ref={Some(field_ref)}"));
        assert!(dropdown.contains("width={props.popup_width}"));
        assert!(dropdown.contains("placement={props.popup_placement}"));
        assert!(dropdown.contains("width_anchor_ref={props.width_anchor_ref.clone()}"));
    }

    #[test]
    fn select_popup_hugs_the_field_and_drops_its_top_left_radius() {
        let source = include_str!("select.rs").split("#[cfg(test)]").next().unwrap();
        let dropdown = include_str!("drop_down_icon_button.rs");
        const { assert!(SELECT_POPUP_GAP_PX < crate::app::components::popup_menu::POPUP_MENU_DEFAULT_GAP_PX) };
        assert!(source.contains("popup_gap={Some(SELECT_POPUP_GAP_PX)}"));
        assert!(source.contains("popup_class={classes!(\"tp__popup-menu--select\")}"));
        assert!(dropdown.contains("gap={props.popup_gap}"));
        assert!(dropdown.contains("class={props.popup_class.clone()}"));

        let styles = include_str!("../../../scss/app/components/_popup_menu.scss");
        let select_rule = styles.split_once("&--select {").unwrap().1.split('}').next().unwrap();
        assert!(select_rule.contains("border-top-left-radius: 0;"));
    }
}
