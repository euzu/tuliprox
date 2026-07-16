use super::{common::CommonInputForm, ConfigInputFormAction, ConfigInputFormState, LABEL_RESOLVE};
use crate::{app::components::ToolAction, hooks::use_service_context, i18n::use_translation};
use shared::model::{ConfigProviderDto, XtreamLoginRequest};
use web_sys::MouseEvent;
use yew::{
    component, html, platform::spawn_local, use_mut_ref, use_state, Callback, Html, Properties, UseReducerHandle,
};

#[derive(Properties, Clone, PartialEq)]
pub(super) struct XtreamInputFormProps {
    pub state: UseReducerHandle<ConfigInputFormState>,
    pub providers: Vec<ConfigProviderDto>,
    pub allow_write: bool,
}

#[component]
pub(super) fn XtreamInputForm(props: &XtreamInputFormProps) -> Html {
    let services = use_service_context();
    let translate = use_translation();
    let loading = use_state(|| false);
    let request_in_flight = use_mut_ref(|| false);
    let request_token = use_mut_ref(|| 0_u64);
    let state = props.state.clone();

    let exp_date_tool_action = {
        let services = services.clone();
        let translate = translate.clone();
        let state = state.clone();
        let providers = props.providers.clone();
        let loading = loading.clone();
        let request_in_flight = request_in_flight.clone();
        let request_token = request_token.clone();
        ToolAction {
            name: Some("RefreshExpDate".to_string()),
            icon: "Refresh".to_string(),
            hint: Some(translate.t(LABEL_RESOLVE)),
            class: (*loading).then(|| "loading".to_string()),
            onclick: Callback::from(move |_event: MouseEvent| {
                if *request_in_flight.borrow() {
                    return;
                }
                let url = state.form.url.clone();
                let username = state.form.username.clone().unwrap_or_default();
                let password = state.form.password.clone().unwrap_or_default();
                if url.trim().is_empty() || username.trim().is_empty() || password.trim().is_empty() {
                    services.toastr.error(translate.t("MESSAGES.SOURCE_EDITOR.URL_USERNAME_AND_PASSWORD_MANDATORY"));
                    return;
                }
                *request_in_flight.borrow_mut() = true;
                let token = {
                    let mut current = request_token.borrow_mut();
                    *current += 1;
                    *current
                };
                loading.set(true);
                let request = XtreamLoginRequest {
                    url,
                    username,
                    password,
                    providers: (!providers.is_empty()).then_some(providers.clone()),
                };
                let services = services.clone();
                let state = state.clone();
                let loading = loading.clone();
                let request_in_flight = request_in_flight.clone();
                let request_token = request_token.clone();
                let translate = translate.clone();
                spawn_local(async move {
                    match services.config.get_xtream_login_info(&request).await {
                        Ok(login_info) if *request_token.borrow() == token => {
                            let unchanged = state.form.url == request.url
                                && state.form.username.as_deref().unwrap_or_default() == request.username
                                && state.form.password.as_deref().unwrap_or_default() == request.password;
                            if unchanged {
                                if let Some(exp_date) = login_info.exp_date {
                                    state.dispatch(ConfigInputFormAction::ExpDate(Some(exp_date)));
                                } else {
                                    services
                                        .toastr
                                        .warning(translate.t("MESSAGES.SOURCE_EDITOR.NO_EXPIRATION_DATE_RETURNED"));
                                }
                            }
                        }
                        Err(err) if *request_token.borrow() == token => services.toastr.error(err.to_string()),
                        Ok(_) | Err(_) => {}
                    }
                    if *request_token.borrow() == token {
                        *request_in_flight.borrow_mut() = false;
                        loading.set(false);
                    }
                });
            }),
        }
    };

    html! {
        <CommonInputForm state={state} allow_write={props.allow_write} credentials={true}
            connection={true} cache_duration={true} exp_date_tool_action={Some(exp_date_tool_action)} />
    }
}
