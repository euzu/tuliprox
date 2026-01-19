use std::sync::Arc;
use crate::app::components::{Card, TextButton};
use crate::{edit_field_date, edit_field_number_i16, edit_field_number_u16, edit_field_text, edit_field_text_option, generate_form_reducer};
use shared::model::ConfigInputAliasDto;
use yew::{function_component, html, use_reducer, Callback, Html, Properties, UseReducerHandle};
use yew_i18n::use_translation;
use shared::utils::Internable;

const LABEL_ALIAS_NAME: &str = "LABEL.ALIAS_NAME";
const LABEL_URL: &str = "LABEL.URL";
const LABEL_USERNAME: &str = "LABEL.USERNAME";
const LABEL_PASSWORD: &str = "LABEL.PASSWORD";
const LABEL_PRIORITY: &str = "LABEL.PRIORITY";
const LABEL_MAX_CONNECTIONS: &str = "LABEL.MAX_CONNECTIONS";
const LABEL_EXP_DATE: &str = "LABEL.EXP_DATE";

generate_form_reducer!(
    state: AliasFormState { form: ConfigInputAliasDto },
    action_name: AliasFormAction,
    fields {
        Name => name: Arc<str>,
        Url => url: String,
        Username => username: Option<String>,
        Password => password: Option<String>,
        Priority => priority: i16,
        MaxConnections => max_connections: u16,
        ExpDate => exp_date: Option<i64>,
    }
);

#[derive(Properties, PartialEq, Clone)]
pub struct AliasItemFormProps {
    pub on_submit: Callback<ConfigInputAliasDto>,
    pub on_cancel: Callback<()>,
    #[prop_or_default]
    pub initial: Option<ConfigInputAliasDto>,
}

#[function_component]
pub fn AliasItemForm(props: &AliasItemFormProps) -> Html {
    let translate = use_translation();

    let form_state: UseReducerHandle<AliasFormState> = use_reducer(|| {
        AliasFormState {
            form: props.initial.clone().unwrap_or_else(|| ConfigInputAliasDto {
                id: 0,
                name: "".intern(),
                url: String::new(),
                username: None,
                password: None,
                priority: 0,
                max_connections: 1,
                exp_date: None,
            }),
            modified: false,
        }
    });

    let handle_submit = {
        let form_state = form_state.clone();
        let on_submit = props.on_submit.clone();
        Callback::from(move |_| {
            let data = form_state.form.clone();
            if !data.name.trim().is_empty() && !data.url.trim().is_empty() {
                on_submit.emit(data);
            }
        })
    };

    let handle_cancel = {
        let on_cancel = props.on_cancel.clone();
        Callback::from(move |_| {
            on_cancel.emit(());
        })
    };

    html! {
        <Card class="tp__config-view__card tp__item-form">
            { edit_field_text!(form_state, translate.t(LABEL_ALIAS_NAME), name, AliasFormAction::Name) }
            { edit_field_text!(form_state, translate.t(LABEL_URL), url, AliasFormAction::Url) }
            <div class="tp__config-view__cols-2">
              { edit_field_text_option!(form_state, translate.t(LABEL_USERNAME), username, AliasFormAction::Username) }
              { edit_field_text_option!(form_state, translate.t(LABEL_PASSWORD), password, AliasFormAction::Password, true) }
              { edit_field_number_i16!(form_state, translate.t(LABEL_PRIORITY), priority, AliasFormAction::Priority) }
              { edit_field_number_u16!(form_state, translate.t(LABEL_MAX_CONNECTIONS), max_connections, AliasFormAction::MaxConnections) }
            </div>
            { edit_field_date!(form_state, translate.t(LABEL_EXP_DATE), exp_date, AliasFormAction::ExpDate) }

            <div class="tp__form-page__toolbar">
                <TextButton
                    class="secondary"
                    name="cancel_alias"
                    icon="Cancel"
                    title={translate.t("LABEL.CANCEL")}
                    onclick={handle_cancel}
                />
                <TextButton
                    class="primary"
                    name="submit_alias"
                    icon="Accept"
                    title={translate.t("LABEL.SUBMIT")}
                    onclick={handle_submit}
                />
            </div>
        </Card>
    }
}
