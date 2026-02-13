use crate::app::components::{
    AppIcon, CollapsePanel, InputTable, PlaylistContext, PlaylistEditorPage, TargetTable,
    TextButton,
};
use crate::app::context::PlaylistEditorContext;
use yew::prelude::*;
use yew_i18n::use_translation;

#[function_component]
pub fn PlaylistList() -> Html {
    let translate = use_translation();
    let playlist_ctx = use_context::<PlaylistContext>().expect("Playlist context not found");
    let playlist_editor_ctx =
        use_context::<PlaylistEditorContext>().expect("PlaylistEditor context not found");

    let handle_create = {
        Callback::from(move |_| {
            playlist_editor_ctx
                .active_page
                .set(PlaylistEditorPage::Create);
        })
    };

    let playlist_body = if let Some(data) = playlist_ctx.sources.as_ref() {
        html! {
            <>
                { for data.iter().map(|(inputs, targets)| html! {
                    <div class="tp__playlist-list__source">
                        <CollapsePanel class="tp__playlist-list__source-inputs" expanded={false} title_content={Some(html!{<><AppIcon name="Input"/>{translate.t("LABEL.INPUTS")}</>})}>
                            <InputTable inputs={Some(inputs.clone())} />
                        </CollapsePanel>
                        <span class="tp__playlist-list__source-label"><AppIcon name="Target" />{translate.t("LABEL.TARGETS")}</span>
                        <TargetTable targets={Some(targets.clone())} />
                    </div>
                }) }
            </>
        }
    } else {
        html! {}
    };

    html! {
      <div class="tp__playlist-list tp__list-list">
        <div class="tp__playlist-list__header tp__list-list__header">
          <h1>{ translate.t("LABEL.PLAYLISTS")}</h1>
          <TextButton class="primary" name="new_playlist"
                icon="PlaylistAdd"
                title={ translate.t("LABEL.NEW_PLAYLIST")}
                onclick={handle_create}></TextButton>
        </div>
        <div class="tp__playlist-list__body tp__list-list__body">
           { playlist_body }
        </div>
      </div>
    }
}
