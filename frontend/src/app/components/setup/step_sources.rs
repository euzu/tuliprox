use super::setup_helpers::{
    build_setup_app_config, collect_setup_warnings, format_setup_error_message, map_sources_to_playlist_rows,
    move_to_next_step, move_to_previous_step, prepare_config_and_api_proxy, prepare_sources,
};
use crate::{
    app::{
        components::{Card, SetupContext, SetupStep, SourceEditor, TextButton},
        context::{ConfigContext, PlaylistContext},
    },
    hooks::use_service_context,
};
use shared::model::SourcesConfigDto;
use std::rc::Rc;
use yew::prelude::*;

#[component]
pub fn SourcesStep() -> Html {
    let setup_ctx = use_context::<SetupContext>().expect("Setup context not found");
    let config_ctx = use_context::<ConfigContext>().expect("ConfigContext not found");
    let services = use_service_context();

    let local_app_config = use_memo(
        (config_ctx.clone(), (*setup_ctx.config_forms).clone(), (*setup_ctx.sources).clone()),
        |(config_ctx, form_state, sources)| build_setup_app_config(config_ctx, form_state, sources.clone()),
    );

    let local_config_context = ConfigContext {
        config: Some(Rc::new((*local_app_config).clone())),
        api_proxy: local_app_config.api_proxy.as_ref().map(|api_proxy| Rc::new(api_proxy.clone())),
    };
    let local_sources =
        use_memo((*local_app_config).clone(), |app_config| Some(map_sources_to_playlist_rows(&app_config.sources)));
    let local_playlist_context = PlaylistContext { sources: local_sources.clone() };

    let handle_sources_change = {
        let setup_ctx = setup_ctx.clone();
        Callback::from(move |sources: SourcesConfigDto| {
            setup_ctx.sources.set(sources);
        })
    };

    let handle_previous = {
        let setup_ctx = setup_ctx.clone();
        Callback::from(move |_| move_to_previous_step(&setup_ctx, SetupStep::Sources))
    };

    let handle_next = {
        let setup_ctx = setup_ctx.clone();
        let config_ctx = config_ctx.clone();
        let services = services.clone();
        Callback::from(move |_| {
            let mut app_config =
                build_setup_app_config(&config_ctx, &setup_ctx.config_forms, (*setup_ctx.sources).clone());
            for warning in collect_setup_warnings(&app_config) {
                services.toastr.warning(warning);
            }

            if let Err(err) = prepare_config_and_api_proxy(&mut app_config) {
                services.toastr.error(format_setup_error_message(err));
                return;
            }

            if let Err(err) = prepare_sources(&mut app_config) {
                services.toastr.error(format_setup_error_message(err));
                return;
            }

            setup_ctx.submit_error.set(None);
            move_to_next_step(&setup_ctx, SetupStep::Sources);
        })
    };

    html! {
        <div class="tp__setup__step tp__setup__step-sources">
            <Card class="tp__setup__step-sources-card">
                <div class="tp__webui-config-view__info tp__config-view-page__info">
                    <span class="info">
                        {"Step 13/16: configure source.yml using the source editor."}
                    </span>
                </div>
                <div class="tp__setup__source-editor-wrap">
                    <ContextProvider<ConfigContext> context={local_config_context}>
                        <ContextProvider<PlaylistContext> context={local_playlist_context}>
                            <SourceEditor show_save_button={false} on_sources_change={Some(handle_sources_change)} />
                        </ContextProvider<PlaylistContext>>
                    </ContextProvider<ConfigContext>>
                </div>
                <div class="tp__config-view__toolbar tp__form-page__toolbar">
                    <TextButton
                        class="secondary"
                        name="setup_sources_previous"
                        icon="ArrowLeft"
                        title={"Back"}
                        onclick={handle_previous}
                    />
                    <TextButton
                        class="primary"
                        name="setup_sources_next"
                        icon="ArrowRight"
                        title={"Next: ApiUsers"}
                        onclick={handle_next}
                    />
                </div>
            </Card>
        </div>
    }
}
