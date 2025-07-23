use yew::prelude::*;
use yew_i18n::use_translation;
use crate::app::components::{PlaylistContext, PlaylistPage, TextButton};

#[function_component]
pub fn PlaylistCreate() -> Html {
    let translate = use_translation();
    let playlist_ctx = use_context::<PlaylistContext>().expect("Playlist context not found");

    let handle_back = {
        let playlist_ctx = playlist_ctx.clone();
        Callback::from(move |_| {
            playlist_ctx.active_page.set(PlaylistPage::List);
        })
    };

    html! {
      <div class="tp__playlist-create tp__list-create">
        <div class="tp__playlist-create__header tp__list-create__header">
           <h1>{ translate.t("LABEL.CREATE")}</h1>
           <TextButton style="primary" name="playlist"
               icon="Playlist"
               title={ translate.t("LABEL.LIST")}
               onclick={handle_back}></TextButton>
        </div>
        <div class="tp__playlist-create__body tp__list-create__body">
        </div>
      </div>
    }
}