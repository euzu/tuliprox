use crate::{
    app::components::{Card, TextButton},
    config_field, config_field_bool, config_field_custom, edit_field_bool, edit_field_number_i16, edit_field_text,
    generate_form_reducer,
    i18n::use_translation,
};
use shared::model::EpgSourceDto;
use yew::{component, html, use_reducer, Callback, Html, Properties, UseReducerHandle};

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
    #[prop_or(false)]
    pub readonly: bool,
}

#[component]
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
            if props.readonly {
                { config_field!(form_state.form, translate.t(LABEL_EPG_SOURCE_URL), url) }
                { config_field_custom!(translate.t(LABEL_EPG_PRIORITY), form_state.form.priority.to_string()) }
                { config_field_bool!(form_state.form, translate.t(LABEL_EPG_LOGO_OVERRIDE), logo_override) }
            } else {
                { edit_field_text!(form_state, translate.t(LABEL_EPG_SOURCE_URL), url, EpgSourceFormAction::Url) }
                { edit_field_number_i16!(form_state, translate.t(LABEL_EPG_PRIORITY), priority, EpgSourceFormAction::Priority) }
                { edit_field_bool!(form_state, translate.t(LABEL_EPG_LOGO_OVERRIDE), logo_override, EpgSourceFormAction::LogoOverride) }
            }

            <div class="tp__form-page__toolbar">
                <TextButton
                    class="secondary"
                    name="cancel_epg_source"
                    icon="Cancel"
                    title={translate.t("LABEL.CANCEL")}
                    onclick={handle_cancel}
                />
                if !props.readonly {
                    <TextButton
                        class="primary"
                        name="submit_epg_source"
                        icon="Accept"
                        title={translate.t("LABEL.SUBMIT")}
                        onclick={handle_submit}
                    />
                }
            </div>
        </Card>
    }
}
