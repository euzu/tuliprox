use yew::prelude::*;
use yew_i18n::use_translation;
use crate::app::components::{PlaylistContext, PlaylistPage, TextButton};

#[function_component(PlaylistCreate)]
pub fn playlist_create() -> Html {
    let translate = use_translation();
    let playlist_ctx = use_context::<PlaylistContext>();

    let handle_back = {
        let playlist_ctx = playlist_ctx.clone();
        Callback::from(move |_| {
            if let Some(ctx) = playlist_ctx.as_ref() {
                ctx.active_page.set(PlaylistPage::Lists);
            }
        })
    };

    html! {
      <div class="tp__playlist-create">
        <div class="tp__playlist-create__header">
           <h1>{ translate.t("LABEL.CREATE")}</h1>
           <TextButton style="primary" name="playlist"
               icon="Playlist"
               title={ translate.t("LABEL.LIST")}
               onclick={handle_back}></TextButton>
        </div>
        <div class="tp__playlist-create__body">
        </div>
      </div>
    }
}