use crate::i18n::use_translation;
use yew::prelude::*;

#[component]
pub fn NoAccess() -> Html {
    let translate = use_translation();
    html! {
        <div class="tp__no-access">
            <h2>{ translate.t("MESSAGES.NO_ACCESS_TITLE") }</h2>
            <p>{ translate.t("MESSAGES.NO_ACCESS_MESSAGE") }</p>
        </div>
    }
}
