use crate::app::components::PlaylistList;
use yew::prelude::*;

#[component]
pub fn PlaylistSettingsView() -> Html {
    html! {
          <div class="tp__playlist-settings-view tp__list-view">
            <div class="tp__playlist-settings-view__body tp__list-view__body">
                <PlaylistList />
            </div>
        </div>
    }
}
