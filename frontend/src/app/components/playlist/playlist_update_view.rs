use crate::app::components::{Breadcrumbs, Card, PlaylistContext, TextButton};
use crate::app::ConfigContext;
use crate::hooks::use_service_context;
use crate::html_if;
use shared::model::ConfigTargetDto;
use std::rc::Rc;
use yew::platform::spawn_local;
use yew::prelude::*;
use yew_hooks::use_list;
use yew_i18n::use_translation;

const LABEL_UPDATE_LOCAL_LIBRARY: &str = "LABEL.UPDATE_LOCAL_LIBRARY";
const ACTION_UPDATE_LIBRARY: &str = "update_library";

#[function_component]
pub fn PlaylistUpdateView() -> Html {
    let translate = use_translation();
    let playlist_ctx = use_context::<PlaylistContext>().expect("Playlist context not found");
    let config_ctx = use_context::<ConfigContext>().expect("Config context not found");
    let services_ctx = use_service_context();
    let breadcrumbs = use_state(|| {
        Rc::new(vec![
            translate.t("LABEL.PLAYLISTS"),
            translate.t("LABEL.UPDATE"),
        ])
    });
    let selected_targets = use_list::<Rc<ConfigTargetDto>>(vec![]);

    let handle_all_select = {
        let selected_targets = selected_targets.clone();
        Callback::from(move |_| {
            selected_targets.clear();
        })
    };

    let handle_target_select = {
        let selected_targets = selected_targets.clone();
        Callback::from(move |target: Rc<ConfigTargetDto>| {
            let exists = selected_targets.current().iter().any(|t| t.id == target.id);
            if !exists {
                selected_targets.push(target);
            } else {
                selected_targets.retain(|t: &Rc<ConfigTargetDto>| t.id != target.id);
            }
        })
    };

    let handle_update = {
        let translate = translate.clone();
        let services = services_ctx.clone();
        let selected_targets = selected_targets.clone();
        Callback::from(move |_| {
            let selected_targets = selected_targets.clone();
            let services = services.clone();
            let translate = translate.clone();
            spawn_local(async move {
                let target_names = {
                    let targets = selected_targets.current();
                    targets
                        .iter()
                        .map(|t| t.name.clone())
                        .collect::<Vec<String>>()
                };
                let update_target_names = target_names
                    .iter()
                    .map(|t| t.as_str())
                    .collect::<Vec<&str>>();
                match services.playlist.update_targets(&update_target_names).await {
                    true => {
                        services
                            .toastr
                            .success(translate.t("MESSAGES.PLAYLIST_UPDATE.SUCCESS"));
                    }
                    false => {
                        services
                            .toastr
                            .error(translate.t("MESSAGES.PLAYLIST_UPDATE.FAIL"));
                    }
                }
            });
        })
    };

    let handle_update_content = {
        let services = services_ctx.clone();
        let translate = translate.clone();
        Callback::from(move |name: String| {
            let services = services.clone();
            let translate = translate.clone();
            wasm_bindgen_futures::spawn_local(async move {
                if name.as_str() == ACTION_UPDATE_LIBRARY {
                    match services.config.update_library().await {
                        Ok(_) => services
                            .toastr
                            .success(translate.t("MESSAGES.LIBRARY_UPDATE.SUCCESS")),
                        Err(_err) => services
                            .toastr
                            .error(translate.t("MESSAGES.LIBRARY_UPDATE.FAIL")),
                    }
                }
            });
        })
    };

    let library_enabled = config_ctx
        .config
        .as_ref()
        .is_some_and(|c| c.config.is_library_enabled());

    html! {
      <div class="tp__playlist-update-view">
         <Breadcrumbs items={&*breadcrumbs}/>
         <div class="tp__playlist-update-view__header">
          <h1>{ translate.t("LABEL.UPDATE")}</h1>
          <div class="tp__config-view__header-tools">
            {html_if!(library_enabled, {
                <TextButton class="tertiary" name={ACTION_UPDATE_LIBRARY}
                    icon="Refresh"
                    title={ translate.t(LABEL_UPDATE_LOCAL_LIBRARY)}
                    onclick={handle_update_content.clone()}></TextButton>
            })}
            </div>
        <TextButton class="primary" name="playlist_update"
               icon="Refresh"
               title={ translate.t("LABEL.UPDATE")}
               onclick={handle_update}></TextButton>
        </div>
        <Card>
         <div class="tp__playlist-update-view__body">
            <TextButton class={if selected_targets.current().is_empty() { "active" } else {""}}
                name={translate.t("LABEL.ALL")} title={translate.t("LABEL.ALL")} icon={"SelectAll"}
                onclick={handle_all_select}/>

         {
            if let Some(data) = playlist_ctx.sources.as_ref() {
              data.iter().flat_map(|(_inputs, targets)| targets)
                    .map(Rc::clone)
                    .map(|target| {
                        let handle_click = handle_target_select.clone();
                        let target_name = target.name.clone();
                        let button_class = if selected_targets.current().iter().any(|t| t.id == target.id) { "active" } else {""};
                        html! {
                          <TextButton class={button_class}
                            name={target_name.clone()} title={target_name} icon={"UpdateChecked"}
                             onclick={move |_| handle_click.emit(target.clone())}/>
                        }
              }).collect::<Html>()
            } else {
              html! {<></>}
            }
         }
         </div>
         </Card>
      </div>
    }
}
