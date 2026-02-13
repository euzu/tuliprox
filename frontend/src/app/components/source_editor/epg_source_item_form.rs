use crate::app::components::{Card, TextButton};
use crate::{edit_field_bool, edit_field_number_i16, edit_field_text, generate_form_reducer};
use shared::model::EpgSourceDto;
use yew::{function_component, html, use_reducer, Callback, Html, Properties, UseReducerHandle};
use yew_i18n::use_translation;

const LABEL_EPG_SOURCE_URL: &str = "LABEL.EPG_SOURCE_URL";
const LABEL_EPG_PRIORITY: &str = "LABEL.PRIORITY";
const LABEL_EPG_LOGO_OVERRIDE: &str = "LABEL.EPG_LOGO_OVERRIDE";

generate_form_reducer!(
    state: EpgSourceFormState { form: EpgSourceDto },
    action_name: EpgSourceFormAction,
    fields {
        Url => url: String,
        Priority => priority: i16,
        LogoOverride => logo_override: bool,
    }
);

#[derive(Properties, PartialEq, Clone)]
pub struct EpgSourceItemFormProps {
    pub on_submit: Callback<EpgSourceDto>,
    pub on_cancel: Callback<()>,
    #[prop_or_default]
    pub initial: Option<EpgSourceDto>,
}

#[function_component]
pub fn EpgSourceItemForm(props: &EpgSourceItemFormProps) -> Html {
    let translate = use_translation();

    let form_state: UseReducerHandle<EpgSourceFormState> = use_reducer(|| EpgSourceFormState {
        form: props.initial.clone().unwrap_or_else(|| EpgSourceDto {
            url: String::new(),
            priority: 0,
            logo_override: false,
        }),
        modified: false,
    });

    let handle_submit = {
        let form_state = form_state.clone();
        let on_submit = props.on_submit.clone();
        Callback::from(move |_| {
            let data = form_state.form.clone();
            if !data.url.trim().is_empty() {
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
            { edit_field_text!(form_state, translate.t(LABEL_EPG_SOURCE_URL), url, EpgSourceFormAction::Url) }
            { edit_field_number_i16!(form_state, translate.t(LABEL_EPG_PRIORITY), priority, EpgSourceFormAction::Priority) }
            { edit_field_bool!(form_state, translate.t(LABEL_EPG_LOGO_OVERRIDE), logo_override, EpgSourceFormAction::LogoOverride) }

            <div class="tp__form-page__toolbar">
                <TextButton
                    class="secondary"
                    name="cancel_epg_source"
                    icon="Cancel"
                    title={translate.t("LABEL.CANCEL")}
                    onclick={handle_cancel}
                />
                <TextButton
                    class="primary"
                    name="submit_epg_source"
                    icon="Accept"
                    title={translate.t("LABEL.SUBMIT")}
                    onclick={handle_submit}
                />
            </div>
        </Card>
    }
}
