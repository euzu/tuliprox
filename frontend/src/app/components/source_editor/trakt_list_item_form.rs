use crate::app::components::select::Select;
use crate::app::components::{Card, DropDownOption, DropDownSelection, TextButton};
use crate::{config_field_child, edit_field_number_u8, edit_field_text, generate_form_reducer};
use shared::model::{TraktContentType, TraktListConfigDto};
use yew::{
    function_component, html, use_memo, use_reducer, Callback, Html, Properties, UseReducerHandle,
};
use yew_i18n::use_translation;

const LABEL_TRAKT_USER: &str = "LABEL.TRAKT_USER";
const LABEL_TRAKT_LIST_SLUG: &str = "LABEL.TRAKT_LIST_SLUG";
const LABEL_TRAKT_CATEGORY_NAME: &str = "LABEL.TRAKT_CATEGORY_NAME";
const LABEL_TRAKT_CONTENT_TYPE: &str = "LABEL.TRAKT_CONTENT_TYPE";
const LABEL_TRAKT_FUZZY_MATCH_THRESHOLD: &str = "LABEL.TRAKT_FUZZY_MATCH_THRESHOLD";

generate_form_reducer!(
    state: TraktListFormState { form: TraktListConfigDto },
    action_name: TraktListFormAction,
    fields {
        User => user: String,
        ListSlug => list_slug: String,
        CategoryName => category_name: String,
        ContentType => content_type: TraktContentType,
        FuzzyMatchThreshold => fuzzy_match_threshold: u8,
    }
);

#[derive(Properties, PartialEq, Clone)]
pub struct TraktListItemFormProps {
    pub on_submit: Callback<TraktListConfigDto>,
    pub on_cancel: Callback<()>,
    #[prop_or_default]
    pub initial: Option<TraktListConfigDto>,
}

#[function_component]
pub fn TraktListItemForm(props: &TraktListItemFormProps) -> Html {
    let translate = use_translation();

    let form_state: UseReducerHandle<TraktListFormState> = use_reducer(|| TraktListFormState {
        form: props.initial.clone().unwrap_or_else(|| TraktListConfigDto {
            user: String::new(),
            list_slug: String::new(),
            category_name: String::new(),
            content_type: TraktContentType::Both,
            fuzzy_match_threshold: 80,
        }),
        modified: false,
    });

    let content_type_options = use_memo(form_state.form.content_type, |content_type| {
        let default_ct = content_type;
        vec![
            DropDownOption {
                id: "vod".to_string(),
                label: html! { "Vod" },
                selected: default_ct == &TraktContentType::Vod,
            },
            DropDownOption {
                id: "series".to_string(),
                label: html! { "Series" },
                selected: default_ct == &TraktContentType::Series,
            },
            DropDownOption {
                id: "both".to_string(),
                label: html! { "Both" },
                selected: default_ct == &TraktContentType::Both,
            },
        ]
    });

    let handle_submit = {
        let form_state = form_state.clone();
        let on_submit = props.on_submit.clone();
        Callback::from(move |_| {
            let data = form_state.form.clone();
            if !data.user.trim().is_empty()
                && !data.list_slug.trim().is_empty()
                && !data.category_name.trim().is_empty()
            {
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
            { edit_field_text!(form_state, translate.t(LABEL_TRAKT_USER), user, TraktListFormAction::User) }
            { edit_field_text!(form_state, translate.t(LABEL_TRAKT_LIST_SLUG), list_slug, TraktListFormAction::ListSlug) }
            { edit_field_text!(form_state, translate.t(LABEL_TRAKT_CATEGORY_NAME), category_name, TraktListFormAction::CategoryName) }

            { config_field_child!(translate.t(LABEL_TRAKT_CONTENT_TYPE), {
                let form_state_ct = form_state.clone();
                html! {
                    <Select
                        name={"trakt_content_type"}
                        multi_select={false}
                        on_select={Callback::from(move |(_, selections):(String, DropDownSelection)| {
                            if let DropDownSelection::Single(option) = selections {
                                if let Ok(ct) = option.parse::<TraktContentType>() {
                                    form_state_ct.dispatch(TraktListFormAction::ContentType(ct));
                                }
                            }
                        })}
                        options={content_type_options.clone()}
                    />
                }
            })}

            { edit_field_number_u8!(form_state, translate.t(LABEL_TRAKT_FUZZY_MATCH_THRESHOLD), fuzzy_match_threshold, TraktListFormAction::FuzzyMatchThreshold) }

            <div class="tp__form-page__toolbar">
                <TextButton
                    class="secondary"
                    name="cancel_trakt_list"
                    icon="Cancel"
                    title={translate.t("LABEL.CANCEL")}
                    onclick={handle_cancel}
                />
                <TextButton
                    class="primary"
                    name="submit_trakt_list"
                    icon="Accept"
                    title={translate.t("LABEL.SUBMIT")}
                    onclick={handle_submit}
                />
            </div>
        </Card>
    }
}
