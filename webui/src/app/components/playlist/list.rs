use yew::prelude::*;
use yew_i18n::use_translation;
use crate::app::components::{PlaylistContext, PlaylistPage, TargetTable, TextButton};

#[function_component]
pub fn PlaylistList() -> Html {
    let translate = use_translation();
    let playlist_ctx = use_context::<PlaylistContext>();

    let handle_create = {
        let playlist_ctx = playlist_ctx.clone();
        Callback::from(move |_| {
            if let Some(ctx) = playlist_ctx.as_ref() {
                ctx.active_page.set(PlaylistPage::NewPlaylist);
            }
        })
    };

    html! {
      <div class="tp__playlist-list">
        <div class="tp__playlist-list__header">
          <h1>{ translate.t("LABEL.PLAYLISTS")}</h1>
          <TextButton style="primary" name="new_playlist"
                icon="PlaylistAdd"
                title={ translate.t("LABEL.NEW_PLAYLIST")}
                onclick={handle_create}></TextButton>
        </div>
        <div class="tp__playlist-create__body">
        <TargetTable/>
        </div>
      </div>
    }
}