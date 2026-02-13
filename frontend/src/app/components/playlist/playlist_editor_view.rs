use crate::app::components::{
    Breadcrumbs, Panel, PlaylistCreate, PlaylistEditorPage, PlaylistList,
};
use crate::app::context::PlaylistEditorContext;
use std::rc::Rc;
use yew::prelude::*;
use yew_i18n::use_translation;

#[function_component]
pub fn PlaylistEditorView() -> Html {
    let translate = use_translation();
    let breadcrumbs = use_state(|| {
        Rc::new(vec![
            translate.t("LABEL.PLAYLISTS"),
            translate.t("LABEL.LIST"),
        ])
    });
    let active_page = use_state(|| PlaylistEditorPage::List);

    let handle_breadcrumb_select = {
        let view_visible = active_page.clone();
        Callback::from(move |(_name, index)| {
            if index == 0 && *view_visible != PlaylistEditorPage::List {
                view_visible.set(PlaylistEditorPage::List);
            }
        })
    };

    {
        let breadcrumbs = breadcrumbs.clone();
        let view_visible_dep = active_page.clone();
        let view_visible = active_page.clone();
        let translate = translate.clone();
        use_effect_with(view_visible_dep, move |_| match *view_visible {
            PlaylistEditorPage::List => breadcrumbs.set(Rc::new(vec![
                translate.t("LABEL.PLAYLISTS"),
                translate.t("LABEL.LIST"),
            ])),
            PlaylistEditorPage::Create => breadcrumbs.set(Rc::new(vec![
                translate.t("LABEL.PLAYLISTS"),
                translate.t("LABEL.CREATE"),
            ])),
        });
    };

    let context = PlaylistEditorContext {
        active_page: active_page.clone(),
    };

    html! {
        <ContextProvider<PlaylistEditorContext> context={context}>
          <div class="tp__playlist-editor-view tp__list-view">
            <Breadcrumbs items={&*breadcrumbs} onclick={ handle_breadcrumb_select }/>
            <div class="tp__playlist-editor-view__body tp__list-view__body">
                <Panel value={PlaylistEditorPage::List.to_string()} active={active_page.to_string()}>
                    <PlaylistList />
                </Panel>
                <Panel value={PlaylistEditorPage::Create.to_string()} active={active_page.to_string()}>
                    <PlaylistCreate />
                </Panel>
            </div>
        </div>
       </ContextProvider<PlaylistEditorContext>>
    }
}
