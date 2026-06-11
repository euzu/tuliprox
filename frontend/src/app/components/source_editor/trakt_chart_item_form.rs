use crate::{
    app::components::{build_options, select::Select, selection_parse_first, Card, DropDownSelection, TextButton},
    config_field_bool, config_field_child, config_field_custom, edit_field_bool, edit_field_number_u8, edit_field_text,
    generate_form_reducer,
    i18n::use_translation,
};
use shared::model::{TraktChartConfigDto, TraktChartKind, TraktChartType};
use yew::{component, html, use_memo, use_reducer, Callback, Html, Properties, UseReducerHandle};

const LABEL_TRAKT_CATEGORY_NAME: &str = "LABEL.TRAKT_CATEGORY_NAME";
const LABEL_TRAKT_TMDB_ONLY: &str = "LABEL.TRAKT_TMDB_ONLY";
const LABEL_TRAKT_FUZZY_MATCH_THRESHOLD: &str = "LABEL.TRAKT_FUZZY_MATCH_THRESHOLD";
const LABEL_TRAKT_CHART_KIND: &str = "LABEL.TRAKT_CHART_KIND";
const LABEL_TRAKT_CHART_TYPE: &str = "LABEL.TRAKT_CHART_TYPE";
const LABEL_TRAKT_CHART_KIND_MOVIES: &str = "LABEL.TRAKT_CHART_KIND_MOVIES";
const LABEL_TRAKT_CHART_KIND_SHOWS: &str = "LABEL.TRAKT_CHART_KIND_SHOWS";
const LABEL_TRAKT_CHART_TYPE_TRENDING: &str = "LABEL.TRAKT_CHART_TYPE_TRENDING";
const LABEL_TRAKT_CHART_TYPE_POPULAR: &str = "LABEL.TRAKT_CHART_TYPE_POPULAR";

fn trakt_chart_kind_label_key(kind: TraktChartKind) -> &'static str {
    match kind {
        TraktChartKind::Movies => LABEL_TRAKT_CHART_KIND_MOVIES,
        TraktChartKind::Shows => LABEL_TRAKT_CHART_KIND_SHOWS,
    }
}

fn trakt_chart_type_label_key(chart: TraktChartType) -> &'static str {
    match chart {
        TraktChartType::Trending => LABEL_TRAKT_CHART_TYPE_TRENDING,
        TraktChartType::Popular => LABEL_TRAKT_CHART_TYPE_POPULAR,
    }
}

generate_form_reducer!(
    state: TraktChartFormState { form: TraktChartConfigDto },
    action_name: TraktChartFormAction,
    fields {
        Kind => kind: TraktChartKind,
        Chart => chart: TraktChartType,
        CategoryName => category_name: String,
        TmdbOnly => tmdb_only: bool,
        FuzzyMatchThreshold => fuzzy_match_threshold: u8,
    }
);

#[derive(Properties, PartialEq, Clone)]
pub struct TraktChartItemFormProps {
    pub on_submit: Callback<TraktChartConfigDto>,
    pub on_cancel: Callback<()>,
    #[prop_or_default]
    pub initial: Option<TraktChartConfigDto>,
    #[prop_or(false)]
    pub readonly: bool,
}

#[component]
pub fn TraktChartItemForm(props: &TraktChartItemFormProps) -> Html {
    let translate = use_translation();

    let form_state: UseReducerHandle<TraktChartFormState> =
        use_reducer(|| TraktChartFormState { form: props.initial.clone().unwrap_or_default(), modified: false });

    let kind_options = use_memo(form_state.form.kind, {
        let translate = translate.clone();
        move |kind| {
            build_options([TraktChartKind::Movies, TraktChartKind::Shows], kind, |value| {
                html! { translate.t(trakt_chart_kind_label_key(*value)) }
            })
        }
    });
    let chart_options = use_memo(form_state.form.chart, {
        let translate = translate.clone();
        move |chart| {
            build_options([TraktChartType::Trending, TraktChartType::Popular], chart, |value| {
                html! { translate.t(trakt_chart_type_label_key(*value)) }
            })
        }
    });

    let handle_submit = {
        let form_state = form_state.clone();
        let on_submit = props.on_submit.clone();
        Callback::from(move |_| {
            let data = form_state.form.clone();
            if !data.category_name.trim().is_empty() {
                on_submit.emit(data);
            }
        })
    };
    let handle_cancel = {
        let on_cancel = props.on_cancel.clone();
        Callback::from(move |_| on_cancel.emit(()))
    };

    html! {
        <Card class="tp__config-view__card tp__item-form">
            if props.readonly {
                { config_field_custom!(translate.t(LABEL_TRAKT_CHART_KIND), translate.t(trakt_chart_kind_label_key(form_state.form.kind))) }
                { config_field_custom!(translate.t(LABEL_TRAKT_CHART_TYPE), translate.t(trakt_chart_type_label_key(form_state.form.chart))) }
                { config_field_custom!(translate.t(LABEL_TRAKT_CATEGORY_NAME), form_state.form.category_name.clone()) }
                { config_field_bool!(form_state.form, translate.t(LABEL_TRAKT_TMDB_ONLY), tmdb_only) }
                { config_field_custom!(translate.t(LABEL_TRAKT_FUZZY_MATCH_THRESHOLD), form_state.form.fuzzy_match_threshold.to_string()) }
            } else {
                { config_field_child!(translate.t(LABEL_TRAKT_CHART_KIND), "TRAKT_CHART_FORM.TRAKT_CHART_KIND", {
                    let form_state_kind = form_state.clone();
                    html! {
                        <Select
                            name={"trakt_chart_kind"}
                            multi_select={false}
                            on_select={Callback::from(move |(_, selections):(String, DropDownSelection)| {
                                if let Some(kind) = selection_parse_first::<TraktChartKind>(&selections) {
                                    form_state_kind.dispatch(TraktChartFormAction::Kind(kind));
                                }
                            })}
                            options={kind_options.clone()}
                        />
                    }
                })}
                { config_field_child!(translate.t(LABEL_TRAKT_CHART_TYPE), "TRAKT_CHART_FORM.TRAKT_CHART_TYPE", {
                    let form_state_chart = form_state.clone();
                    html! {
                        <Select
                            name={"trakt_chart_type"}
                            multi_select={false}
                            on_select={Callback::from(move |(_, selections):(String, DropDownSelection)| {
                                if let Some(chart) = selection_parse_first::<TraktChartType>(&selections) {
                                    form_state_chart.dispatch(TraktChartFormAction::Chart(chart));
                                }
                            })}
                            options={chart_options.clone()}
                        />
                    }
                })}
                { edit_field_text!(form_state, translate.t(LABEL_TRAKT_CATEGORY_NAME), category_name, TraktChartFormAction::CategoryName) }
                { edit_field_bool!(form_state, translate.t(LABEL_TRAKT_TMDB_ONLY), tmdb_only, TraktChartFormAction::TmdbOnly) }
                { edit_field_number_u8!(form_state, translate.t(LABEL_TRAKT_FUZZY_MATCH_THRESHOLD), fuzzy_match_threshold, TraktChartFormAction::FuzzyMatchThreshold) }
            }

            <div class="tp__form-page__toolbar">
                <TextButton
                    class="secondary"
                    name="cancel_trakt_chart"
                    icon="Cancel"
                    title={translate.t("LABEL.CANCEL")}
                    onclick={handle_cancel}
                />
                if !props.readonly {
                    <TextButton
                        class="primary"
                        name="submit_trakt_chart"
                        icon="Accept"
                        title={translate.t("LABEL.SUBMIT")}
                        onclick={handle_submit}
                    />
                }
            </div>
        </Card>
    }
}
