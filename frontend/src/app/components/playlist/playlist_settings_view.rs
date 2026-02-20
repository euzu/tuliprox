use crate::app::components::{Breadcrumbs, PlaylistList};
use std::rc::Rc;
use yew::prelude::*;
use yew_i18n::use_translation;

#[function_component]
pub fn PlaylistSettingsView() -> Html {
    let translate = use_translation();
    let breadcrumbs = use_state(|| Rc::new(vec![translate.t("LABEL.PLAYLISTS"), translate.t("LABEL.LIST")]));

    html! {
          <div class="tp__playlist-settings-view tp__list-view">
            <Breadcrumbs items={&*breadcrumbs}/>
            <div class="tp__playlist-settings-view__body tp__list-view__body">
                <PlaylistList />
            </div>
        </div>
    }
}
