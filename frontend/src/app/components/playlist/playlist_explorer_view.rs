use crate::{
    app::{
        components::{playlist::playlist_explorer::PlaylistExplorer, Breadcrumbs, PlaylistSourceSelector},
        context::PlaylistExplorerContext,
    },
    i18n::use_translation,
};
use shared::model::{PlaylistRequest, UiPlaylistCategories};
use std::rc::Rc;
use yew::prelude::*;

#[component]
pub fn PlaylistExplorerView() -> Html {
    let translate = use_translation();
    let breadcrumbs = use_state(|| Rc::new(vec![translate.t("LABEL.PLAYLISTS"), translate.t("LABEL.EXPLORER")]));
    let playlist = use_state(|| None::<Rc<UiPlaylistCategories>>);
    let playlist_req = use_state(|| None::<PlaylistRequest>);

    let context = PlaylistExplorerContext { playlist: playlist.clone(), playlist_request: playlist_req.clone() };

    html! {
        <ContextProvider<PlaylistExplorerContext> context={context}>
          <div class="tp__playlist-explorer-view tp__list-view">
            <Breadcrumbs items={&*breadcrumbs}/>
            <div class="tp__playlist-explorer-view__header">
                <h1>{ translate.t("LABEL.EXPLORER") }</h1>
            </div>
            <div class="tp__playlist-explorer-view__body tp__list-view__body">
                <div class={"tp__panel"}>
                    <PlaylistSourceSelector hide_title=true />
                    <PlaylistExplorer />
                </div>
            </div>
        </div>
       </ContextProvider<PlaylistExplorerContext>>
    }
}
