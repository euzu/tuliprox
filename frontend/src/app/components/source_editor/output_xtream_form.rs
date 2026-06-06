use crate::{
    app::components::{
        config::HasFormData, BlockId, BlockInstance, Card, EditMode, FilterInput, IconButton, Panel,
        SourceEditorContext, TextButton, TitledCard, TraktChartItemForm, TraktListItemForm,
    },
    config_field, config_field_bool, config_field_child, config_field_custom, edit_field_bool, edit_field_text,
    generate_form_reducer,
    i18n::use_translation,
};
use shared::{
    concat_string,
    error::TuliproxError,
    model::{
        TargetOutputDto, TraktApiConfigDto, TraktChartConfigDto, TraktConfigDto, TraktListConfigDto,
        XtreamTargetOutputDto,
    },
    utils::Internable,
};
use std::{fmt::Display, rc::Rc, str::FromStr, sync::Arc};
use web_sys::MouseEvent;
use yew::{
    component, html, use_context, use_effect_with, use_reducer, use_state, Callback, Html, Properties, UseReducerHandle,
};

const LABEL_SKIP_DIRECT_SOURCE: &str = "LABEL.SKIP_DIRECT_SOURCE";
const LABEL_LIVE: &str = "LABEL.LIVE";
const LABEL_VOD: &str = "LABEL.VOD";
const LABEL_SERIES: &str = "LABEL.SERIES";
const LABEL_FILTER: &str = "LABEL.FILTER";
const LABEL_TRAKT_API_KEY: &str = "LABEL.API_KEY";
const LABEL_TRAKT_API_VERSION: &str = "LABEL.API_VERSION";
const LABEL_TRAKT_API_URL: &str = "LABEL.API_URL";
const LABEL_TRAKT_LISTS: &str = "LABEL.TRAKT_LISTS";
const LABEL_ADD_TRAKT_LIST: &str = "LABEL.ADD_TRAKT_LIST";
const LABEL_TRAKT_CHARTS: &str = "LABEL.TRAKT_CHARTS";
const LABEL_ADD_TRAKT_CHART: &str = "LABEL.ADD_TRAKT_CHART";
const LABEL_API_CONFIGURATION: &str = "LABEL.API_CONFIGURATION";
const LABEL_USER_AGENT: &str = "LABEL.API_USER_AGENT";
const LABEL_MAIN: &str = "LABEL.MAIN_CONFIG";
const LABEL_TRAKT: &str = "LABEL.TRAKT";
const LABEL_ENABLED: &str = "LABEL.ENABLED";

#[derive(Copy, Clone, PartialEq, Eq)]
enum XtreamOutputFormPage {
    Main,
    Trakt,
}

impl XtreamOutputFormPage {
    const MAIN: &str = "Main";
    const TRAKT: &str = "Trakt";
}

impl FromStr for XtreamOutputFormPage {
    type Err = TuliproxError;

    fn from_str(s: &str) -> Result<Self, TuliproxError> {
        match s {
            Self::MAIN => Ok(XtreamOutputFormPage::Main),
            Self::TRAKT => Ok(XtreamOutputFormPage::Trakt),
            _ => Err(TuliproxError::Config(format!("Unknown xtream output form page: {s}"))),
        }
    }
}

impl Display for XtreamOutputFormPage {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(
            f,
            "{}",
            match *self {
                XtreamOutputFormPage::Main => Self::MAIN,
                XtreamOutputFormPage::Trakt => Self::TRAKT,
            }
        )
    }
}

impl Internable for XtreamOutputFormPage {
    fn intern(self) -> Arc<str> {
        match self {
            Self::Main => Self::MAIN,
            Self::Trakt => Self::TRAKT,
        }
        .intern()
    }
}

generate_form_reducer!(
    state: TraktConfigFormState { form: TraktConfigDto },
    action_name: TraktConfigFormAction,
    fields {
        Enabled => enabled: bool,
    }
);

generate_form_reducer!(
    state: TraktApiConfigFormState { form: TraktApiConfigDto },
    action_name: TraktApiConfigFormAction,
    fields {
        ApiKey => api_key: String,
        Version => version: String,
        Url => url: String,
        UserAgent => user_agent: String,
    }
);

generate_form_reducer!(
    state: XtreamTargetOutputFormState { form: XtreamTargetOutputDto },
    action_name: XtreamTargetOutputFormAction,
    fields {
        SkipLiveDirectSource => skip_live_direct_source: bool,
        SkipVideoDirectSource => skip_video_direct_source: bool,
        SkipSeriesDirectSource =>  skip_series_direct_source: bool,
        Filter => filter: Option<String>,
    }
);

#[derive(Properties, PartialEq, Clone)]
pub struct XtreamTargetOutputViewProps {
    pub(crate) block_id: BlockId,
    pub(crate) output: Option<Rc<XtreamTargetOutputDto>>,
    #[prop_or(true)]
    pub(crate) allow_write: bool,
}

fn build_trakt_output_config(
    enabled: bool,
    api: TraktApiConfigDto,
    lists: Vec<TraktListConfigDto>,
    charts: Vec<TraktChartConfigDto>,
) -> Option<TraktConfigDto> {
    if lists.is_empty() && charts.is_empty() {
        None
    } else {
        Some(TraktConfigDto { enabled, api, lists, charts })
    }
}

fn append_trakt_matching_summary_suffix(mut summary: String, tmdb_only: bool) -> String {
    if tmdb_only {
        summary.push_str(", TMDB only");
    }
    summary
}

fn trakt_list_summary(item: &TraktListConfigDto) -> String {
    append_trakt_matching_summary_suffix(
        format!(
            "{} / {} - {} ({}, {}%)",
            item.user, item.list_slug, item.category_name, item.content_type, item.fuzzy_match_threshold
        ),
        item.tmdb_only,
    )
}

fn trakt_chart_summary(item: &TraktChartConfigDto) -> String {
    append_trakt_matching_summary_suffix(
        format!("{}/{} - {} ({}%)", item.kind, item.chart, item.category_name, item.fuzzy_match_threshold),
        item.tmdb_only,
    )
}

fn upsert_vec_item<T>(items: &mut Vec<T>, index: Option<usize>, item: T) {
    if let Some(index) = index.filter(|idx| *idx < items.len()) {
        items[index] = item;
    } else {
        items.push(item);
    }
}

fn adjust_edit_index_after_remove(current: Option<usize>, removed: usize) -> Option<usize> {
    match current {
        Some(index) if index == removed => None,
        Some(index) if index > removed => Some(index - 1),
        _ => current,
    }
}

#[component]
pub fn XtreamTargetOutputView(props: &XtreamTargetOutputViewProps) -> Html {
    let translate = use_translation();
    let source_editor_ctx = use_context::<SourceEditorContext>().expect("SourceEditorContext not found");

    let output_form_state: UseReducerHandle<XtreamTargetOutputFormState> =
        use_reducer(|| XtreamTargetOutputFormState { form: XtreamTargetOutputDto::default(), modified: false });

    let trakt_state: UseReducerHandle<TraktConfigFormState> =
        use_reducer(|| TraktConfigFormState { form: TraktConfigDto::default(), modified: false });

    let trakt_api_state: UseReducerHandle<TraktApiConfigFormState> =
        use_reducer(|| TraktApiConfigFormState { form: TraktApiConfigDto::default(), modified: false });

    let trakt_lists_state = use_state(Vec::<TraktListConfigDto>::new);
    let trakt_charts_state = use_state(Vec::<TraktChartConfigDto>::new);
    let show_trakt_list_form_state = use_state(|| false);
    let show_trakt_chart_form_state = use_state(|| false);
    let editing_trakt_list_index_state = use_state(|| None::<usize>);
    let editing_trakt_chart_index_state = use_state(|| None::<usize>);

    let view_visible = use_state(|| XtreamOutputFormPage::Main);

    let handle_menu_click = {
        let active_menu = view_visible.clone();
        Callback::from(move |(name, _): (String, _)| {
            if let Ok(view_type) = XtreamOutputFormPage::from_str(&name) {
                active_menu.set(view_type);
            }
        })
    };

    {
        let output_form_state = output_form_state.clone();
        let trakt_state = trakt_state.clone();
        let trakt_api_state = trakt_api_state.clone();
        let trakt_lists_state = trakt_lists_state.clone();
        let trakt_charts_state = trakt_charts_state.clone();
        let editing_trakt_list_index_state = editing_trakt_list_index_state.clone();
        let editing_trakt_chart_index_state = editing_trakt_chart_index_state.clone();
        let show_trakt_list_form_state = show_trakt_list_form_state.clone();
        let show_trakt_chart_form_state = show_trakt_chart_form_state.clone();

        let config_output = props.output.clone();

        use_effect_with(config_output, move |cfg| {
            if let Some(target) = cfg {
                output_form_state.dispatch(XtreamTargetOutputFormAction::SetAll(target.as_ref().clone()));

                // Load Trakt configuration
                if let Some(trakt) = &target.trakt {
                    trakt_state.dispatch(TraktConfigFormAction::SetAll(trakt.clone()));
                    trakt_api_state.dispatch(TraktApiConfigFormAction::SetAll(trakt.api.clone()));
                    trakt_lists_state.set(trakt.lists.clone());
                    trakt_charts_state.set(trakt.charts.clone());
                } else {
                    trakt_state.dispatch(TraktConfigFormAction::SetAll(TraktConfigDto::default()));
                    trakt_api_state.dispatch(TraktApiConfigFormAction::SetAll(TraktApiConfigDto::default()));
                    trakt_lists_state.set(Vec::new());
                    trakt_charts_state.set(Vec::new());
                }
            } else {
                output_form_state.dispatch(XtreamTargetOutputFormAction::SetAll(XtreamTargetOutputDto::default()));
                trakt_state.dispatch(TraktConfigFormAction::SetAll(TraktConfigDto::default()));
                trakt_api_state.dispatch(TraktApiConfigFormAction::SetAll(TraktApiConfigDto::default()));
                trakt_lists_state.set(Vec::new());
                trakt_charts_state.set(Vec::new());
            }
            editing_trakt_list_index_state.set(None);
            editing_trakt_chart_index_state.set(None);
            show_trakt_list_form_state.set(false);
            show_trakt_chart_form_state.set(false);
            || ()
        });
    }

    let handle_add_trakt_list_item = {
        let trakt_list = trakt_lists_state.clone();
        let show_trakt_list_form = show_trakt_list_form_state.clone();
        let editing_trakt_list_index_state = editing_trakt_list_index_state.clone();

        Callback::from(move |item: TraktListConfigDto| {
            let mut items = (*trakt_list).clone();
            upsert_vec_item(&mut items, *editing_trakt_list_index_state, item);
            trakt_list.set(items);
            editing_trakt_list_index_state.set(None);
            show_trakt_list_form.set(false);
        })
    };

    let handle_remove_trakt_list_item = {
        let trakt_list = trakt_lists_state.clone();
        let editing_trakt_list_index_state = editing_trakt_list_index_state.clone();
        Callback::from(move |(idx, _e): (String, MouseEvent)| {
            if let Ok(index) = idx.parse::<usize>() {
                let mut items = (*trakt_list).clone();
                if index < items.len() {
                    items.remove(index);
                    trakt_list.set(items);
                    editing_trakt_list_index_state
                        .set(adjust_edit_index_after_remove(*editing_trakt_list_index_state, index));
                }
            }
        })
    };

    let handle_edit_trakt_list_item = {
        let show_trakt_list_form = show_trakt_list_form_state.clone();
        let editing_trakt_list_index_state = editing_trakt_list_index_state.clone();
        Callback::from(move |(idx, _e): (String, MouseEvent)| {
            if let Ok(index) = idx.parse::<usize>() {
                editing_trakt_list_index_state.set(Some(index));
                show_trakt_list_form.set(true);
            }
        })
    };

    let handle_add_trakt_chart_item = {
        let trakt_charts = trakt_charts_state.clone();
        let show_trakt_chart_form = show_trakt_chart_form_state.clone();
        let editing_trakt_chart_index_state = editing_trakt_chart_index_state.clone();

        Callback::from(move |item: TraktChartConfigDto| {
            let mut items = (*trakt_charts).clone();
            upsert_vec_item(&mut items, *editing_trakt_chart_index_state, item);
            trakt_charts.set(items);
            editing_trakt_chart_index_state.set(None);
            show_trakt_chart_form.set(false);
        })
    };

    let handle_remove_trakt_chart_item = {
        let trakt_charts = trakt_charts_state.clone();
        let editing_trakt_chart_index_state = editing_trakt_chart_index_state.clone();
        Callback::from(move |(idx, _e): (String, MouseEvent)| {
            if let Ok(index) = idx.parse::<usize>() {
                let mut items = (*trakt_charts).clone();
                if index < items.len() {
                    items.remove(index);
                    trakt_charts.set(items);
                    editing_trakt_chart_index_state
                        .set(adjust_edit_index_after_remove(*editing_trakt_chart_index_state, index));
                }
            }
        })
    };

    let handle_edit_trakt_chart_item = {
        let show_trakt_chart_form = show_trakt_chart_form_state.clone();
        let editing_trakt_chart_index_state = editing_trakt_chart_index_state.clone();
        Callback::from(move |(idx, _e): (String, MouseEvent)| {
            if let Ok(index) = idx.parse::<usize>() {
                editing_trakt_chart_index_state.set(Some(index));
                show_trakt_chart_form.set(true);
            }
        })
    };

    let handle_close_trakt_list_form = {
        let show_trakt_list_form = show_trakt_list_form_state.clone();
        let editing_trakt_list_index_state = editing_trakt_list_index_state.clone();
        Callback::from(move |()| {
            editing_trakt_list_index_state.set(None);
            show_trakt_list_form.set(false);
        })
    };

    let handle_close_trakt_chart_form = {
        let show_trakt_chart_form = show_trakt_chart_form_state.clone();
        let editing_trakt_chart_index_state = editing_trakt_chart_index_state.clone();
        Callback::from(move |()| {
            editing_trakt_chart_index_state.set(None);
            show_trakt_chart_form.set(false);
        })
    };

    let handle_show_trakt_list_form = {
        let show_trakt_list_form = show_trakt_list_form_state.clone();
        let editing_trakt_list_index_state = editing_trakt_list_index_state.clone();
        Callback::from(move |_name| {
            editing_trakt_list_index_state.set(None);
            show_trakt_list_form.set(true);
        })
    };

    let handle_show_trakt_chart_form = {
        let show_trakt_chart_form = show_trakt_chart_form_state.clone();
        let editing_trakt_chart_index_state = editing_trakt_chart_index_state.clone();
        Callback::from(move |_name| {
            editing_trakt_chart_index_state.set(None);
            show_trakt_chart_form.set(true);
        })
    };

    let render_output = || {
        let output_form_state_1 = output_form_state.clone();
        if !props.allow_write {
            html! {
                <Card class="tp__config-view__card">
                    <TitledCard title={translate.t(LABEL_SKIP_DIRECT_SOURCE)}>
                        <div class="tp__config-view__cols-3">
                            { config_field_bool!(output_form_state.form, translate.t(LABEL_LIVE), skip_live_direct_source) }
                            { config_field_bool!(output_form_state.form, translate.t(LABEL_VOD), skip_video_direct_source) }
                            { config_field_bool!(output_form_state.form, translate.t(LABEL_SERIES), skip_series_direct_source) }
                        </div>
                    </TitledCard>
                    { config_field_custom!(
                        translate.t(LABEL_FILTER),
                        output_form_state.form.filter.clone().unwrap_or_default()
                    ) }
                </Card>
            }
        } else {
            html! {
                <Card class="tp__config-view__card">
                    <TitledCard title={translate.t(LABEL_SKIP_DIRECT_SOURCE)}>
                      <div class="tp__config-view__cols-3">
                      { edit_field_bool!(output_form_state, translate.t(LABEL_LIVE), skip_live_direct_source,  XtreamTargetOutputFormAction::SkipLiveDirectSource) }
                      { edit_field_bool!(output_form_state, translate.t(LABEL_VOD), skip_video_direct_source,  XtreamTargetOutputFormAction::SkipVideoDirectSource) }
                      { edit_field_bool!(output_form_state, translate.t(LABEL_SERIES), skip_series_direct_source,  XtreamTargetOutputFormAction::SkipSeriesDirectSource) }
                      </div>
                    </TitledCard>
                    { config_field_child!(translate.t(LABEL_FILTER), "OUTPUT_XTREAM_FORM.FILTER", {
                           html! {
                                <FilterInput filter={output_form_state_1.form.filter.clone()} on_change={Callback::from(move |new_filter| {
                                    output_form_state_1.dispatch(XtreamTargetOutputFormAction::Filter(new_filter));
                                })} />
                           }
                    })}
                </Card>
            }
        }
    };

    let render_trakt = || {
        let trakt_lists = trakt_lists_state.clone();
        let trakt_charts = trakt_charts_state.clone();
        let trakt_form = trakt_state.clone();
        let trakt_api_form = trakt_api_state.clone();
        let show_trakt_list_form = show_trakt_list_form_state.clone();
        let show_trakt_chart_form = show_trakt_chart_form_state.clone();
        let editing_trakt_list_index = *editing_trakt_list_index_state;
        let editing_trakt_chart_index = *editing_trakt_chart_index_state;
        let initial_trakt_list = editing_trakt_list_index.and_then(|index| (*trakt_lists).get(index).cloned());
        let initial_trakt_chart = editing_trakt_chart_index.and_then(|index| (*trakt_charts).get(index).cloned());

        html! {
            <Card class="tp__config-view__card">
                if *show_trakt_list_form {
                    <TraktListItemForm
                        on_submit={handle_add_trakt_list_item}
                        on_cancel={handle_close_trakt_list_form}
                        initial={initial_trakt_list}
                        readonly={!props.allow_write}
                    />
                } else if *show_trakt_chart_form {
                    <TraktChartItemForm
                        on_submit={handle_add_trakt_chart_item}
                        on_cancel={handle_close_trakt_chart_form}
                        initial={initial_trakt_chart}
                        readonly={!props.allow_write}
                    />
                } else {
                { if props.allow_write {
                    html! { { edit_field_bool!(trakt_form, translate.t(LABEL_ENABLED), enabled, TraktConfigFormAction::Enabled) } }
                } else {
                    html! { { config_field_bool!(trakt_form.form, translate.t(LABEL_ENABLED), enabled) } }
                }}
                <div class="tp__form-section">
                    <h3>{translate.t(LABEL_API_CONFIGURATION)}</h3>
                    if props.allow_write {
                        <>
                            { edit_field_text!(trakt_api_form, translate.t(LABEL_TRAKT_API_KEY), api_key, TraktApiConfigFormAction::ApiKey) }
                            { edit_field_text!(trakt_api_form, translate.t(LABEL_TRAKT_API_VERSION), version, TraktApiConfigFormAction::Version) }
                            { edit_field_text!(trakt_api_form, translate.t(LABEL_TRAKT_API_URL), url, TraktApiConfigFormAction::Url) }
                            { edit_field_text!(trakt_api_form, translate.t(LABEL_USER_AGENT), user_agent, TraktApiConfigFormAction::UserAgent) }
                        </>
                    } else {
                        <>
                            { config_field!(trakt_api_form.form, translate.t(LABEL_TRAKT_API_KEY), api_key) }
                            { config_field!(trakt_api_form.form, translate.t(LABEL_TRAKT_API_VERSION), version) }
                            { config_field!(trakt_api_form.form, translate.t(LABEL_TRAKT_API_URL), url) }
                            { config_field!(trakt_api_form.form, translate.t(LABEL_USER_AGENT), user_agent) }
                        </>
                    }
                </div>

                // Trakt Lists
                { config_field_child!(translate.t(LABEL_TRAKT_LISTS), "OUTPUT_XTREAM_FORM.TRAKT_LISTS", {
                    let trakt_lists_list = trakt_lists.clone();
                    html! {
                        <div class="tp__form-list">
                            <div class="tp__form-list__items">
                            for item in (*trakt_lists_list).iter().enumerate() {
                                <div class="tp__form-list__item" key={format!("trakt-{}", item.0)}>
                                    if props.allow_write {
                                        <div class="tp__form-list__item-toolbar">
                                            <IconButton
                                                class="tp__form-list__item-edit"
                                                name={item.0.to_string()}
                                                icon="Edit"
                                                onclick={handle_edit_trakt_list_item.clone()}/>
                                            <IconButton
                                                name={item.0.to_string()}
                                                icon="Delete"
                                                onclick={handle_remove_trakt_list_item.clone()}/>
                                        </div>
                                    }
                                    <div class="tp__form-list__item-content">
                                        <span>{trakt_list_summary(item.1)}</span>
                                    </div>
                                </div>
                            }
                            </div>

                            if props.allow_write {
                                <TextButton
                                    class="primary"
                                    name="add_trakt_list"
                                    icon="Add"
                                    title={translate.t(LABEL_ADD_TRAKT_LIST)}
                                    onclick={handle_show_trakt_list_form}
                                />
                            }
                        </div>
                    }
                })}
                { config_field_child!(translate.t(LABEL_TRAKT_CHARTS), "OUTPUT_XTREAM_FORM.TRAKT_CHARTS", {
                    let trakt_charts_list = trakt_charts.clone();
                    html! {
                        <div class="tp__form-list">
                            <div class="tp__form-list__items">
                            for item in (*trakt_charts_list).iter().enumerate() {
                                <div class="tp__form-list__item" key={format!("trakt-chart-{}", item.0)}>
                                    if props.allow_write {
                                        <div class="tp__form-list__item-toolbar">
                                            <IconButton
                                                class="tp__form-list__item-edit"
                                                name={item.0.to_string()}
                                                icon="Edit"
                                                onclick={handle_edit_trakt_chart_item.clone()}/>
                                            <IconButton
                                                name={item.0.to_string()}
                                                icon="Delete"
                                                onclick={handle_remove_trakt_chart_item.clone()}/>
                                        </div>
                                    }
                                    <div class="tp__form-list__item-content">
                                        <span>{trakt_chart_summary(item.1)}</span>
                                    </div>
                                </div>
                            }
                            </div>

                            if props.allow_write {
                                <TextButton
                                    class="primary"
                                    name="add_trakt_chart"
                                    icon="Add"
                                    title={translate.t(LABEL_ADD_TRAKT_CHART)}
                                    onclick={handle_show_trakt_chart_form}
                                />
                            }
                        </div>
                    }
                })}
            }
            </Card>
        }
    };

    let render_edit_mode = || {
        html! {
            <div class="tp__input-form__body">
            <div class="tp__input-form__body__pages">
                <Panel value={XtreamOutputFormPage::Main.intern()} active={view_visible.intern()}>
                {render_output()}
                </Panel>
                <Panel value={XtreamOutputFormPage::Trakt.intern()} active={view_visible.intern()}>
                {render_trakt()}
                </Panel>
            </div>
            </div>
        }
    };

    let button_disabled = *show_trakt_list_form_state || *show_trakt_chart_form_state;

    let render_sidebar = || {
        let main_class = format!(
            "tp__app-sidebar-menu--{}{}",
            XtreamOutputFormPage::Main,
            if *view_visible == XtreamOutputFormPage::Main { " active" } else { "" }
        );
        let trakt_class = format!(
            "tp__app-sidebar-menu--{}{}",
            XtreamOutputFormPage::Trakt,
            if *view_visible == XtreamOutputFormPage::Trakt { " active" } else { "" }
        );
        html! {
        <div class={concat_string!("tp__source-editor-form__sidebar", if button_disabled {" disabled"} else {""})}>
            <IconButton class={main_class} icon="Settings" hint={translate.t(LABEL_MAIN)} name={XtreamOutputFormPage::Main.to_string()} onclick={&handle_menu_click}></IconButton>
            <IconButton class={trakt_class} icon="Trakt" hint={translate.t(LABEL_TRAKT)} name={XtreamOutputFormPage::Trakt.to_string()} onclick={&handle_menu_click}></IconButton>
        </div>
        }
    };

    let handle_apply_target = {
        let source_editor_ctx = source_editor_ctx.clone();
        let output_form_state = output_form_state.clone();
        let trakt_state = trakt_state.clone();
        let trakt_api_state = trakt_api_state.clone();
        let trakt_lists_state = trakt_lists_state.clone();
        let trakt_charts_state = trakt_charts_state.clone();
        let block_id = props.block_id;
        Callback::from(move |_| {
            let mut output = output_form_state.data().clone();

            let trakt_lists = (*trakt_lists_state).clone();
            let trakt_charts = (*trakt_charts_state).clone();
            output.trakt = build_trakt_output_config(
                trakt_state.data().enabled,
                trakt_api_state.data().clone(),
                trakt_lists,
                trakt_charts,
            );

            source_editor_ctx
                .on_form_change
                .emit((block_id, BlockInstance::Output(Rc::new(TargetOutputDto::Xtream(output)))));
            source_editor_ctx.edit_mode.set(EditMode::Inactive);
        })
    };
    let handle_cancel = {
        let source_editor_ctx = source_editor_ctx.clone();
        Callback::from(move |_| {
            source_editor_ctx.edit_mode.set(EditMode::Inactive);
        })
    };

    html! {
        <div class="tp__source-editor-form tp__config-view-page">
          <div class="tp__source-editor-form__toolbar tp__form-page__toolbar">
             <TextButton class={concat_string!("secondary", if button_disabled {" disabled"} else {""} )} name="cancel_input"
                icon="Cancel"
                title={ translate.t("LABEL.CANCEL")}
                onclick={handle_cancel}></TextButton>
             if props.allow_write {
                 <TextButton class={concat_string!("primary", if button_disabled {" disabled"} else {""} )} name="apply_input"
                    icon="Accept"
                    title={ translate.t("LABEL.OK")}
                    onclick={handle_apply_target}></TextButton>
             }
          </div>
          <div class="tp__source-editor-form__content">
                { render_sidebar() }
                { render_edit_mode() }
          </div>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared::model::{TraktChartKind, TraktChartType};

    #[test]
    fn build_trakt_output_config_returns_none_when_empty() {
        let result = build_trakt_output_config(true, TraktApiConfigDto::default(), Vec::new(), Vec::new());
        assert!(result.is_none());
    }

    #[test]
    fn build_trakt_output_config_keeps_charts_without_lists() {
        let charts = vec![TraktChartConfigDto {
            kind: TraktChartKind::Movies,
            chart: TraktChartType::Trending,
            category_name: "Trending Movies".to_string(),
            tmdb_only: true,
            fuzzy_match_threshold: 90,
        }];

        let result = build_trakt_output_config(true, TraktApiConfigDto::default(), Vec::new(), charts.clone())
            .expect("charts-only trakt config");

        assert!(result.lists.is_empty());
        assert_eq!(result.charts, charts);
    }

    #[test]
    fn upsert_vec_item_replaces_existing_entry() {
        let mut items = vec!["a".to_string(), "b".to_string()];
        upsert_vec_item(&mut items, Some(1), "c".to_string());
        assert_eq!(items, vec!["a", "c"]);
    }

    #[test]
    fn upsert_vec_item_appends_when_index_missing() {
        let mut items = vec!["a".to_string()];
        upsert_vec_item(&mut items, Some(5), "b".to_string());
        assert_eq!(items, vec!["a", "b"]);
    }

    #[test]
    fn adjust_edit_index_after_remove_clears_exact_match() {
        assert_eq!(adjust_edit_index_after_remove(Some(2), 2), None);
    }

    #[test]
    fn adjust_edit_index_after_remove_shifts_later_item_left() {
        assert_eq!(adjust_edit_index_after_remove(Some(2), 0), Some(1));
        assert_eq!(adjust_edit_index_after_remove(Some(3), 1), Some(2));
    }
}
