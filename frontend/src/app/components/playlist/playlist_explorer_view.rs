use crate::{
    app::{
        components::{
            playlist::playlist_explorer::PlaylistExplorer, recording::target_name_for_id, Breadcrumbs,
            PlaylistSourceSelector,
        },
        context::{ConfigContext, PlaylistExplorerContext},
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
    let error = use_state(|| None::<String>);

    let config = use_context::<ConfigContext>().and_then(|ctx| ctx.config);
    let target_name = use_state(|| None::<String>);
    let stale_target = match playlist_req.as_ref() {
        Some(PlaylistRequest::Target(id)) => {
            let current = config.as_ref().and_then(|cfg| target_name_for_id(&cfg.sources, *id, None));
            target_name.is_none() || current != *target_name
        }
        _ => false,
    };
    let context = PlaylistExplorerContext { playlist, playlist_request: playlist_req, error, target_name };

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
                    { stale_target.then(|| html! { <p role="alert">{translate.t("MESSAGES.CONFIG_CHANGED")}</p> }) }
                    <div hidden={stale_target}>
                        <PlaylistExplorer {stale_target} />
                    </div>
                </div>
            </div>
        </div>
       </ContextProvider<PlaylistExplorerContext>>
    }
}
