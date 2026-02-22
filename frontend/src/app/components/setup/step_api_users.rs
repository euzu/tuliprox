use super::setup_helpers::{
    apply_setup_api_users, build_setup_app_config, collect_setup_warnings, format_setup_error_message,
    map_sources_to_playlist_rows, move_to_next_step, move_to_previous_step, prepare_config_and_api_proxy,
    prepare_sources,
};
use crate::{
    app::{
        components::{Card, SetupContext, SetupStep, TextButton, UserlistView},
        context::{ConfigContext, PlaylistContext},
    },
    hooks::use_service_context,
};
use shared::model::TargetUserDto;
use std::rc::Rc;
use yew::prelude::*;

#[function_component]
pub fn ApiUsersStep() -> Html {
    let setup_ctx = use_context::<SetupContext>().expect("Setup context not found");
    let config_ctx = use_context::<ConfigContext>().expect("ConfigContext not found");
    let services = use_service_context();

    let local_app_config = use_memo(
        (
            config_ctx.clone(),
            (*setup_ctx.config_forms).clone(),
            (*setup_ctx.sources).clone(),
            (*setup_ctx.api_users).clone(),
        ),
        |(config_ctx, form_state, sources, api_users)| {
            let mut app_config = build_setup_app_config(config_ctx, form_state, sources.clone());
            apply_setup_api_users(&mut app_config, api_users);
            app_config
        },
    );

    let sources =
        use_memo((*local_app_config).clone(), |app_config| Some(map_sources_to_playlist_rows(&app_config.sources)));

    let local_config_context = ConfigContext {
        config: Some(Rc::new((*local_app_config).clone())),
        api_proxy: local_app_config.api_proxy.as_ref().map(|api_proxy| Rc::new(api_proxy.clone())),
    };
    let local_playlist_context = PlaylistContext { sources: sources.clone() };

    let handle_users_change = {
        let setup_ctx = setup_ctx.clone();
        Callback::from(move |users: Vec<TargetUserDto>| {
            setup_ctx.api_users.set(users);
        })
    };

    let handle_previous = {
        let setup_ctx = setup_ctx.clone();
        Callback::from(move |_| move_to_previous_step(&setup_ctx, SetupStep::ApiUsers))
    };

    let handle_next = {
        let setup_ctx = setup_ctx.clone();
        let config_ctx = config_ctx.clone();
        let services = services.clone();
        Callback::from(move |_| {
            let mut app_config =
                build_setup_app_config(&config_ctx, &setup_ctx.config_forms, (*setup_ctx.sources).clone());
            apply_setup_api_users(&mut app_config, setup_ctx.api_users.as_ref());

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
            move_to_next_step(&setup_ctx, SetupStep::ApiUsers);
        })
    };

    html! {
        <div class="tp__setup__step tp__setup__step-api-users">
            <Card>
                <div class="tp__config-view__header">
                    <h1>{"API Users"}</h1>
                </div>
                <div class="tp__config-view__body">
                    <div class="tp__webui-config-view__info tp__config-view-page__info">
                        <span class="info">
                            {"Step 14/16: create API users for playlists and token-based access (api-proxy.yml)."}
                        </span>
                    </div>
                    <ContextProvider<ConfigContext> context={local_config_context}>
                        <ContextProvider<PlaylistContext> context={local_playlist_context}>
                            <div class="tp__setup__userlist-wrap">
                                <UserlistView
                                    local_mode={true}
                                    users={Some((*setup_ctx.api_users).clone())}
                                    on_users_change={Some(handle_users_change)}
                                />
                            </div>
                        </ContextProvider<PlaylistContext>>
                    </ContextProvider<ConfigContext>>
                </div>
                <div class="tp__config-view__toolbar tp__form-page__toolbar">
                    <TextButton
                        class="secondary"
                        name="setup_api_users_previous"
                        icon="ArrowLeft"
                        title={"Back"}
                        onclick={handle_previous}
                    />
                    <TextButton
                        class="primary"
                        name="setup_api_users_next"
                        icon="ArrowRight"
                        title={"Next: Schedules"}
                        onclick={handle_next}
                    />
                </div>
            </Card>
        </div>
    }
}
