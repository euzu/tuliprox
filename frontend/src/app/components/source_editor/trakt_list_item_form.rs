use crate::{
    app::components::{build_options, select::Select, selection_parse_first, Card, DropDownSelection, TextButton},
    config_field, config_field_bool, config_field_child, config_field_custom, edit_field_bool, edit_field_number_u8,
    edit_field_text, generate_form_reducer,
    i18n::use_translation,
};
use shared::model::{TraktContentType, TraktListConfigDto};
use yew::{component, html, use_memo, use_reducer, Callback, Html, Properties, UseReducerHandle};

const LABEL_TRAKT_USER: &str = "LABEL.TRAKT_USER";
const LABEL_TRAKT_LIST_SLUG: &str = "LABEL.TRAKT_LIST_SLUG";
const LABEL_TRAKT_CATEGORY_NAME: &str = "LABEL.TRAKT_CATEGORY_NAME";
const LABEL_TRAKT_CONTENT_TYPE: &str = "LABEL.TRAKT_CONTENT_TYPE";
const LABEL_TRAKT_TMDB_ONLY: &str = "LABEL.TRAKT_TMDB_ONLY";
const LABEL_TRAKT_FUZZY_MATCH_THRESHOLD: &str = "LABEL.TRAKT_FUZZY_MATCH_THRESHOLD";
const LABEL_TRAKT_CONTENT_TYPE_VOD: &str = "LABEL.TRAKT_CONTENT_TYPE_VOD";
const LABEL_TRAKT_CONTENT_TYPE_SERIES: &str = "LABEL.TRAKT_CONTENT_TYPE_SERIES";
const LABEL_TRAKT_CONTENT_TYPE_BOTH: &str = "LABEL.TRAKT_CONTENT_TYPE_BOTH";

fn trakt_content_type_label_key(content_type: TraktContentType) -> &'static str {
    match content_type {
        TraktContentType::Vod => LABEL_TRAKT_CONTENT_TYPE_VOD,
        TraktContentType::Series => LABEL_TRAKT_CONTENT_TYPE_SERIES,
        TraktContentType::Both => LABEL_TRAKT_CONTENT_TYPE_BOTH,
    }
}

generate_form_reducer!(
    state: TraktListFormState { form: TraktListConfigDto },
    action_name: TraktListFormAction,
    fields {
        User => user: String,
        ListSlug => list_slug: String,
        CategoryName => category_name: String,
        ContentType => content_type: TraktContentType,
        TmdbOnly => tmdb_only: bool,
        FuzzyMatchThreshold => fuzzy_match_threshold: u8,
    }
);

#[derive(Properties, PartialEq, Clone)]
pub struct TraktListItemFormProps {
    pub on_submit: Callback<TraktListConfigDto>,
    pub on_cancel: Callback<()>,
    #[prop_or_default]
    pub initial: Option<TraktListConfigDto>,
    #[prop_or(false)]
    pub readonly: bool,
}

#[component]
pub fn TraktListItemForm(props: &TraktListItemFormProps) -> Html {
    let translate = use_translation();

    let form_state: UseReducerHandle<TraktListFormState> = use_reducer(|| TraktListFormState {
        form: props.initial.clone().unwrap_or_else(|| TraktListConfigDto {
            user: String::new(),
            list_slug: String::new(),
            category_name: String::new(),
            content_type: TraktContentType::Both,
            tmdb_only: false,
            fuzzy_match_threshold: 80,
        }),
        modified: false,
    });

    let content_type_options = use_memo(form_state.form.content_type, {
        let translate = translate.clone();
        move |content_type| {
            build_options(
                [TraktContentType::Vod, TraktContentType::Series, TraktContentType::Both],
                content_type,
                |value| html! { translate.t(trakt_content_type_label_key(*value)) },
            )
        }
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
            if props.readonly {
                { config_field!(form_state.form, translate.t(LABEL_TRAKT_USER), user) }
                { config_field!(form_state.form, translate.t(LABEL_TRAKT_LIST_SLUG), list_slug) }
                { config_field!(form_state.form, translate.t(LABEL_TRAKT_CATEGORY_NAME), category_name) }
            } else {
                <>
                    { edit_field_text!(form_state, translate.t(LABEL_TRAKT_USER), user, TraktListFormAction::User) }
                    { edit_field_text!(form_state, translate.t(LABEL_TRAKT_LIST_SLUG), list_slug, TraktListFormAction::ListSlug) }
                    { edit_field_text!(form_state, translate.t(LABEL_TRAKT_CATEGORY_NAME), category_name, TraktListFormAction::CategoryName) }
                </>
            }

            if props.readonly {
                { config_field_custom!(translate.t(LABEL_TRAKT_CONTENT_TYPE), translate.t(trakt_content_type_label_key(form_state.form.content_type))) }
            } else {
                { config_field_child!(translate.t(LABEL_TRAKT_CONTENT_TYPE), "TRAKT_LIST_FORM.TRAKT_CONTENT_TYPE", {
                    let form_state_ct = form_state.clone();
                    html! {
                        <Select
                            name={"trakt_content_type"}
                            multi_select={false}
                            on_select={Callback::from(move |(_, selections):(String, DropDownSelection)| {
                                if let Some(ct) = selection_parse_first::<TraktContentType>(&selections) {
                                    form_state_ct.dispatch(TraktListFormAction::ContentType(ct));
                                }
                            })}
                            options={content_type_options.clone()}
                        />
                    }
                })}
            }

            if props.readonly {
                { config_field_bool!(form_state.form, translate.t(LABEL_TRAKT_TMDB_ONLY), tmdb_only) }
            } else {
                { edit_field_bool!(form_state, translate.t(LABEL_TRAKT_TMDB_ONLY), tmdb_only, TraktListFormAction::TmdbOnly) }
            }

            if props.readonly {
                { config_field_custom!(
                    translate.t(LABEL_TRAKT_FUZZY_MATCH_THRESHOLD),
                    form_state.form.fuzzy_match_threshold.to_string()
                ) }
            } else {
                { edit_field_number_u8!(form_state, translate.t(LABEL_TRAKT_FUZZY_MATCH_THRESHOLD), fuzzy_match_threshold, TraktListFormAction::FuzzyMatchThreshold) }
            }

            <div class="tp__form-page__toolbar">
                <TextButton
                    class="secondary"
                    name="cancel_trakt_list"
                    icon="Cancel"
                    title={translate.t("LABEL.CANCEL")}
                    onclick={handle_cancel}
                />
                if !props.readonly {
                    <TextButton
                        class="primary"
                        name="submit_trakt_list"
                        icon="Accept"
                        title={translate.t("LABEL.SUBMIT")}
                        onclick={handle_submit}
                    />
                }
            </div>
        </Card>
    }
}
