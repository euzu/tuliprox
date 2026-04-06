use crate::app::{
    components::{playlist::playlist_explorer::PlaylistExplorer, PlaylistSourceSelector},
    context::PlaylistExplorerContext,
};
use shared::model::{PlaylistRequest, UiPlaylistCategories};
use std::rc::Rc;
use yew::prelude::*;

#[component]
pub fn PlaylistExplorerView() -> Html {
    let playlist = use_state(|| None::<Rc<UiPlaylistCategories>>);
    let playlist_req = use_state(|| None::<PlaylistRequest>);

    let context = PlaylistExplorerContext { playlist: playlist.clone(), playlist_request: playlist_req.clone() };

    html! {
        <ContextProvider<PlaylistExplorerContext> context={context}>
          <div class="tp__playlist-explorer-view tp__list-view">
            <div class="tp__playlist-explorer-view__body tp__list-view__body">
                <div class={"tp__panel"}>
                    <PlaylistSourceSelector />
                    <PlaylistExplorer />
                </div>
            </div>
        </div>
       </ContextProvider<PlaylistExplorerContext>>
    }
}
