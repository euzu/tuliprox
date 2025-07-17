use crate::app::components::{Breadcrumbs, InputRow, Panel, PlaylistContext, PlaylistCreate, PlaylistList, PlaylistPage};
use std::rc::Rc;
use yew::prelude::*;
use yew_i18n::use_translation;
use crate::app::context::ConfigContext;

#[function_component]
pub fn PlaylistView() -> Html {
    let translate = use_translation();
    let breadcrumbs = use_state(|| Rc::new(vec![translate.t("LABEL.PLAYLISTS"), translate.t("LABEL.LIST")]));
    let active_page = use_state(|| PlaylistPage::List);
    let config_ctx = use_context::<ConfigContext>().expect("Config context not found");

    let handle_breadcrumb_select = {
        let view_visible = active_page.clone();
        Callback::from(move |(_name, index)| {
            if index == 0 && *view_visible != PlaylistPage::List {
                view_visible.set(PlaylistPage::List);
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
                PlaylistPage::List => breadcrumbs.set(Rc::new(vec![translate.t("LABEL.PLAYLISTS"), translate.t("LABEL.LIST")])),
                PlaylistPage::Create => breadcrumbs.set(Rc::new(vec![translate.t("LABEL.PLAYLISTS"), translate.t("LABEL.CREATE")])),
            }
        });
    };

    let sources = use_memo(config_ctx.config.map(|c| c.sources.clone()), |cfg_sources_opt| {
        if let Some(cfg_sources) = cfg_sources_opt {
            let mut sources = vec![];
            for source in &cfg_sources.sources {
                let mut inputs = vec![];
                for input_cfg in &source.inputs {
                    let input = Rc::new(input_cfg.clone());
                    inputs.push(Rc::new(InputRow::Input(Rc::clone(&input))));
                    if let Some(aliases) = input_cfg.aliases.as_ref() {
                        for alias in aliases {
                            inputs.push(Rc::new(InputRow::Alias(Rc::new(alias.clone()), Rc::clone(&input))));
                        }
                    }
                }
                let mut targets = vec![];
                for target in &source.targets {
                    targets.push(Rc::new(target.clone()));
                }
                sources.push((inputs, targets));
            }
            Some(Rc::new(sources))
        } else {
            None
        }
    });

    let context = PlaylistContext {
        sources,
        active_page: active_page.clone(),
    };

    html! {
        <ContextProvider<PlaylistContext> context={context}>
          <div class="tp__playlist-view tp__list-view">
            <Breadcrumbs items={&*breadcrumbs} onclick={ handle_breadcrumb_select }/>
            <div class="tp__playlist-view__body tp__list-view__body">
                <Panel value={PlaylistPage::List.to_string()} active={active_page.to_string()}>
                    <PlaylistList />
                </Panel>
                <Panel value={PlaylistPage::Create.to_string()} active={active_page.to_string()}>
                    <PlaylistCreate />
                </Panel>
            </div>
        </div>
       </ContextProvider<PlaylistContext>>
    }
}