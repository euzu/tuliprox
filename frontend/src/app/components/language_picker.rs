use crate::{
    app::components::{menu_item::MenuItem, popup_menu::PopupMenu, IconButton},
    i18n::{use_translation, LanguageState},
};
use web_sys::MouseEvent;
use yew::{function_component, html, use_context, use_state, Callback, Html, NodeRef};

#[function_component(LanguagePicker)]
pub fn language_picker() -> Html {
    let translate = use_translation();
    let language_state = use_context::<LanguageState>();

    let button_ref = NodeRef::default();
    let popup_anchor_ref = use_state(|| None::<web_sys::Element>);
    let popup_is_open = use_state(|| false);

    let Some(language_state) = language_state else {
        return html! {};
    };

    // Nothing to choose from when only a single language is available.
    if language_state.languages.len() < 2 {
        return html! {};
    }

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

    let handle_language_select = {
        let popup_is_open = popup_is_open.clone();
        let on_change = language_state.on_change.clone();
        Callback::from(move |(code, event): (String, MouseEvent)| {
            event.prevent_default();
            event.stop_propagation();
            on_change.emit(code);
            popup_is_open.set(false);
        })
    };

    let active_label = language_state
        .languages
        .iter()
        .find(|l| l.code == language_state.active)
        .map_or_else(|| language_state.active.to_uppercase(), crate::i18n::LanguageInfo::display_label);

    html! {
        <>
            <IconButton
                button_ref={Some(button_ref)}
                name="Language"
                icon="Language"
                hint={format!("{}: {}", translate.t("LABEL.LANGUAGE"), active_label)}
                onclick={handle_popup_open}
            />
            <PopupMenu is_open={*popup_is_open} anchor_ref={(*popup_anchor_ref).clone()} on_close={handle_popup_close}>
                {
                    for language_state.languages.iter().map(|lang| {
                        let class = if lang.code == language_state.active {
                            "tp__language-picker__item active".to_owned()
                        } else {
                            "tp__language-picker__item".to_owned()
                        };
                        html! {
                            <MenuItem
                                name={lang.code.clone()}
                                label={lang.display_label()}
                                class={class}
                                onclick={handle_language_select.clone()}
                            />
                        }
                    })
                }
            </PopupMenu>
        </>
    }
}
