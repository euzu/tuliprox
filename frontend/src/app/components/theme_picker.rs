use crate::app::components::{menu_item::MenuItem, popup_menu::PopupMenu, theme::Theme, IconButton};
use web_sys::MouseEvent;
use yew::{component, html, use_state, Callback, Html, NodeRef, Properties};

#[derive(Properties, Clone, PartialEq)]
pub struct ThemePickerProps {
    pub theme: Theme,
    pub on_select: Callback<Theme>,
}

#[component]
pub fn ThemePicker(props: &ThemePickerProps) -> Html {
    let button_ref = NodeRef::default();
    let popup_anchor_ref = use_state(|| None::<web_sys::Element>);
    let popup_is_open = use_state(|| false);

    let handle_popup_open = {
        let button_ref = button_ref.clone();
        let popup_anchor_ref = popup_anchor_ref.clone();
        let popup_is_open = popup_is_open.clone();
        Callback::from(move |(_name, event): (String, MouseEvent)| {
            event.prevent_default();
            event.stop_propagation();
            if let Some(button) = button_ref.cast::<web_sys::Element>() {
                popup_anchor_ref.set(Some(button));
                popup_is_open.set(true);
            }
        })
    };

    let handle_popup_close = {
        let popup_is_open = popup_is_open.clone();
        Callback::from(move |()| popup_is_open.set(false))
    };

    let handle_theme_select = {
        let popup_is_open = popup_is_open.clone();
        let on_select = props.on_select.clone();
        Callback::from(move |(theme_name, event): (String, MouseEvent)| {
            event.prevent_default();
            event.stop_propagation();
            if let Ok(theme) = theme_name.parse::<Theme>() {
                on_select.emit(theme);
                popup_is_open.set(false);
            }
        })
    };

    html! {
        <>
            <IconButton
                button_ref={Some(button_ref)}
                name="Theme"
                icon={if props.theme.is_light() { "Moon" } else { "Sun" }}
                hint={format!("Theme: {}", props.theme.label())}
                onclick={handle_popup_open}
            />
            <PopupMenu is_open={*popup_is_open} anchor_ref={(*popup_anchor_ref).clone()} on_close={handle_popup_close}>
                {
                    for Theme::all().iter().copied().map(|theme| html! {
                        <MenuItem
                            name={theme.to_string()}
                            label={theme.label()}
                            icon={if theme.is_light() { "Sun".to_owned() } else { "Moon".to_owned() }}
                            class={if theme == props.theme { "tp__theme-picker__item active".to_owned() } else { "tp__theme-picker__item".to_owned() }}
                            onclick={handle_theme_select.clone()}
                        />
                    })
                }
            </PopupMenu>
        </>
    }
}
