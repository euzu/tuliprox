use crate::app::components::{Breadcrumbs, Panel, PlaylistContext, PlaylistCreate, PlaylistList, PlaylistPage};
use std::rc::Rc;
use yew::prelude::*;
use yew_i18n::use_translation;

#[function_component]
pub fn PlaylistView() -> Html {
    let translate = use_translation();
    let breadcrumbs = use_state(|| Rc::new(vec![translate.t("LABEL.PLAYLISTS"), translate.t("LABEL.LIST")]));
    let active_page = use_state(|| PlaylistPage::Lists);

    let handle_breadcrumb_select = {
        let view_visible = active_page.clone();
        Callback::from(move |(_name, index)| {
            if index == 0 && *view_visible != PlaylistPage::Lists {
                view_visible.set(PlaylistPage::Lists);
            }
        })
    };

    {
        let breadcrumbs = breadcrumbs.clone();
        let view_visible_dep = active_page.clone();
        let view_visible = active_page.clone();
        let translate = translate.clone();
        use_effect_with(view_visible_dep, move |_| {
            match *view_visible {
                PlaylistPage::Lists => breadcrumbs.set(Rc::new(vec![translate.t("LABEL.PLAYLISTS"), translate.t("LABEL.LIST")])),
                PlaylistPage::NewPlaylist => breadcrumbs.set(Rc::new(vec![translate.t("LABEL.PLAYLISTS"), translate.t("LABEL.CREATE")])),
            }
        });
    };

    let context = PlaylistContext {
        active_page: active_page.clone(),
    };

    html! {
        <ContextProvider<PlaylistContext> context={context}>
          <div class="tp__playlist-view">
            <Breadcrumbs items={&*breadcrumbs} onclick={ handle_breadcrumb_select }/>
            <div class="tp__playlist-view__body">
                <Panel value={PlaylistPage::Lists.to_string()} active={active_page.to_string()}>
                    <PlaylistList />
                </Panel>
                <Panel value={PlaylistPage::NewPlaylist.to_string()} active={active_page.to_string()}>
                    <PlaylistCreate />
                </Panel>
            </div>
        </div>
       </ContextProvider<PlaylistContext>>
    }
}