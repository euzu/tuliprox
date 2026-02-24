use crate::{
    app::{
        components::{
            playlist::playlist_explorer::PlaylistExplorer, Breadcrumbs, Panel, PlaylistExplorerPage,
            PlaylistSourceSelector,
        },
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
    let active_page = use_state(|| PlaylistExplorerPage::SourceSelector);
    let playlist = use_state(|| None::<Rc<UiPlaylistCategories>>);
    let playlist_req = use_state(|| None::<PlaylistRequest>);
    let breadcrumbs = match *active_page {
        PlaylistExplorerPage::SourceSelector => {
            Rc::new(vec![translate.t("LABEL.PLAYLISTS"), translate.t("LABEL.SOURCES")])
        }
    };

    let handle_breadcrumb_select = {
        let view_visible = active_page.clone();
        Callback::from(move |(_name, index)| {
            if index == 0 && *view_visible != PlaylistExplorerPage::SourceSelector {
                view_visible.set(PlaylistExplorerPage::SourceSelector);
            }
        })
    };

    let context = PlaylistExplorerContext {
        active_page: active_page.clone(),
        playlist: playlist.clone(),
        playlist_request: playlist_req.clone(),
    };

    html! {
        <ContextProvider<PlaylistExplorerContext> context={context}>
          <div class="tp__playlist-explorer-view tp__list-view">
            <Breadcrumbs items={breadcrumbs.clone()} onclick={ handle_breadcrumb_select }/>
            <div class="tp__playlist-explorer-view__body tp__list-view__body">
                <Panel value={PlaylistExplorerPage::SourceSelector.to_string()} active={active_page.to_string()}>
                    <PlaylistSourceSelector />
                    <PlaylistExplorer />
                </Panel>
            </div>
        </div>
       </ContextProvider<PlaylistExplorerContext>>
    }
}
