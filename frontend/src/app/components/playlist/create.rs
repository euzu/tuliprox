use crate::app::components::{PlaylistEditorContext, PlaylistEditorPage, TextButton};
use yew::prelude::*;
use yew_i18n::use_translation;

#[function_component]
pub fn PlaylistCreate() -> Html {
    let translate = use_translation();
    let playlist_editor_ctx =
        use_context::<PlaylistEditorContext>().expect("PlaylistEditor context not found");

    let handle_back = {
        Callback::from(move |_| {
            playlist_editor_ctx
                .active_page
                .set(PlaylistEditorPage::List);
        })
    };

    html! {
      <div class="tp__playlist-create tp__list-create">
        <div class="tp__playlist-create__header tp__list-create__header">
           <h1>{ translate.t("LABEL.CREATE")}</h1>
           <TextButton class="primary" name="playlist"
               icon="Playlist"
               title={ translate.t("LABEL.LIST")}
               onclick={handle_back}></TextButton>
        </div>
        <div class="tp__playlist-create__body tp__list-create__body">
        </div>
      </div>
    }
}
