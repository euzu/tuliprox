use crate::app::components::config::HasFormData;
use crate::app::components::{BlockId, BlockInstance, Card, EditMode, IconButton, Panel, SourceEditorContext, TextButton, TraktListItemForm, TitledCard, FilterInput};
use crate::{config_field_child, edit_field_bool, edit_field_number_u16, edit_field_text, generate_form_reducer};
use shared::model::{TargetOutputDto, TraktApiConfigDto, TraktConfigDto, TraktContentType, TraktListConfigDto, XtreamTargetOutputDto};
use std::fmt::Display;
use std::rc::Rc;
use std::str::FromStr;
use web_sys::MouseEvent;
use yew::{function_component, html, use_context, use_effect_with, use_reducer, use_state, Callback, Html, Properties, UseReducerHandle};
use yew_i18n::use_translation;
use shared::error::TuliproxError;
use shared::info_err_res;

const LABEL_SKIP_DIRECT_SOURCE: &str = "LABEL.SKIP_DIRECT_SOURCE";
const LABEL_LIVE: &str = "LABEL.LIVE";
const LABEL_VOD: &str = "LABEL.VOD";
const LABEL_SERIES: &str = "LABEL.SERIES";
const LABEL_RESOLVE: &str = "LABEL.RESOLVE";
const LABEL_RESOLVE_DELAY_SEC: &str = "LABEL.RESOLVE_DELAY_SEC";
const LABEL_FILTER: &str = "LABEL.FILTER";
const LABEL_TRAKT_API_KEY: &str = "LABEL.API_KEY";
const LABEL_TRAKT_API_VERSION: &str = "LABEL.API_VERSION";
const LABEL_TRAKT_API_URL: &str = "LABEL.API_URL";
const LABEL_TRAKT_LISTS: &str = "LABEL.TRAKT_LISTS";
const LABEL_ADD_TRAKT_LIST: &str = "LABEL.ADD_TRAKT_LIST";
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
            _ => info_err_res!("Unknown xtream output form page: {s}"),
        }
    }
}

impl Display for XtreamOutputFormPage {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{}", match *self {
            XtreamOutputFormPage::Main => Self::MAIN,
            XtreamOutputFormPage::Trakt => Self::TRAKT,
        })
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
        ResolveSeries =>  resolve_series: bool,
        ResolveSeriesDelay =>  resolve_series_delay: u16,
        ResolveVod =>  resolve_vod: bool,
        ResolveVodDelay =>  resolve_vod_delay: u16,
        Filter => filter: Option<String>,
    }
);

#[derive(Properties, PartialEq, Clone)]
pub struct XtreamTargetOutputViewProps {
    pub(crate) block_id: BlockId,
    pub(crate) output: Option<Rc<XtreamTargetOutputDto>>,
}

#[function_component]
pub fn XtreamTargetOutputView(props: &XtreamTargetOutputViewProps) -> Html {
    let translate = use_translation();
    let source_editor_ctx = use_context::<SourceEditorContext>().expect("SourceEditorContext not found");

    let output_form_state: UseReducerHandle<XtreamTargetOutputFormState> =
        use_reducer(|| XtreamTargetOutputFormState {
            form: XtreamTargetOutputDto::default(),
            modified: false,
        });

    let trakt_state: UseReducerHandle<TraktConfigFormState> =
        use_reducer(|| TraktConfigFormState {
            form: TraktConfigDto::default(),
            modified: false,
        });

    let trakt_api_state: UseReducerHandle<TraktApiConfigFormState> =
        use_reducer(|| TraktApiConfigFormState {
            form: TraktApiConfigDto::default(),
            modified: false,
        });

    // State for Trakt lists
    let trakt_lists_state = use_state(Vec::<TraktListConfigDto>::new);

    // State for showing trakt list form
    let show_trakt_list_form_state = use_state(|| false);

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

        let config_output = props.output.clone();

        use_effect_with(config_output, move |cfg| {
            if let Some(target) = cfg {
                output_form_state.dispatch(XtreamTargetOutputFormAction::SetAll(target.as_ref().clone()));

                // Load Trakt configuration
                if let Some(trakt) = &target.trakt {
                    trakt_state.dispatch(TraktConfigFormAction::SetAll(trakt.clone()));
                    trakt_api_state.dispatch(TraktApiConfigFormAction::SetAll(trakt.api.clone()));
                    trakt_lists_state.set(trakt.lists.clone());
                } else {
                    trakt_state.dispatch(TraktConfigFormAction::SetAll(TraktConfigDto::default()));
                    trakt_api_state.dispatch(TraktApiConfigFormAction::SetAll(TraktApiConfigDto::default()));
                    trakt_lists_state.set(Vec::new());
                }
            } else {
                output_form_state.dispatch(XtreamTargetOutputFormAction::SetAll(XtreamTargetOutputDto::default()));
                trakt_state.dispatch(TraktConfigFormAction::SetAll(TraktConfigDto::default()));
                trakt_api_state.dispatch(TraktApiConfigFormAction::SetAll(TraktApiConfigDto::default()));
                trakt_lists_state.set(Vec::new());
            }
            || ()
        });
    }

    let handle_add_trakt_list_item = {
        let trakt_list = trakt_lists_state.clone();
        let show_trakt_list_form = show_trakt_list_form_state.clone();

        Callback::from(move |item: TraktListConfigDto| {
            let mut items = (*trakt_list).clone();
            items.push(item);
            trakt_list.set(items);
            show_trakt_list_form.set(false);
        })
    };

    let handle_remove_trakt_list_item = {
        let trakt_list = trakt_lists_state.clone();
        Callback::from(move |(idx, _e): (String, MouseEvent)| {
            if let Ok(index) = idx.parse::<usize>() {
                let mut items = (*trakt_list).clone();
                if index < items.len() {
                    items.remove(index);
                    trakt_list.set(items);
                }
            }
        })
    };

    let handle_close_trakt_list_form = {
        let show_trakt_list_form = show_trakt_list_form_state.clone();
        Callback::from(move |()| {
            show_trakt_list_form.set(false);
        })
    };

    let handle_show_trakt_list_form = {
        let show_trakt_list_form = show_trakt_list_form_state.clone();
        Callback::from(move |_name| {
            show_trakt_list_form.set(true);
        })
    };

    let render_output = || {
        let output_form_state_1 = output_form_state.clone();
        html! {
            <Card class="tp__config-view__card">
                <TitledCard title={translate.t(LABEL_SKIP_DIRECT_SOURCE)}>
                  <div class="tp__config-view__cols-3">
                  { edit_field_bool!(output_form_state, translate.t(LABEL_LIVE), skip_live_direct_source,  XtreamTargetOutputFormAction::SkipLiveDirectSource) }
                  { edit_field_bool!(output_form_state, translate.t(LABEL_VOD), skip_video_direct_source,  XtreamTargetOutputFormAction::SkipVideoDirectSource) }
                  { edit_field_bool!(output_form_state, translate.t(LABEL_SERIES), skip_series_direct_source,  XtreamTargetOutputFormAction::SkipSeriesDirectSource) }
                  </div>
                </TitledCard>
                <TitledCard title={translate.t(LABEL_RESOLVE)}>
                    <div class="tp__config-view__cols-2">
                    { edit_field_bool!(output_form_state, translate.t(LABEL_VOD), resolve_vod,  XtreamTargetOutputFormAction::ResolveVod) }
                    { edit_field_bool!(output_form_state, translate.t(LABEL_SERIES), resolve_series,  XtreamTargetOutputFormAction::ResolveSeries) }
                    </div>
                </TitledCard>
                <TitledCard title={translate.t(LABEL_RESOLVE_DELAY_SEC)}>
                    <div class="tp__config-view__cols-2">
                    { edit_field_number_u16!(output_form_state, translate.t(LABEL_VOD), resolve_vod_delay,  XtreamTargetOutputFormAction::ResolveVodDelay) }
                    { edit_field_number_u16!(output_form_state, translate.t(LABEL_SERIES), resolve_series_delay,  XtreamTargetOutputFormAction::ResolveSeriesDelay) }
                    </div>
                </TitledCard>
                { config_field_child!(translate.t(LABEL_FILTER), {
                       html! {
                            <FilterInput filter={output_form_state_1.form.filter.clone()} on_change={Callback::from(move |new_filter| {
                                output_form_state_1.dispatch(XtreamTargetOutputFormAction::Filter(new_filter));
                            })} />
                       }
                })}
            </Card>
        }
    };

    let render_trakt = || {
        let trakt_lists = trakt_lists_state.clone();
        let trakt_form = trakt_state.clone();
        let trakt_api_form = trakt_api_state.clone();
        let show_trakt_list_form = show_trakt_list_form_state.clone();

        html! {
            <Card class="tp__config-view__card">
                if *show_trakt_list_form {
                    <TraktListItemForm
                        on_submit={handle_add_trakt_list_item}
                        on_cancel={handle_close_trakt_list_form}
                    />
                } else {
                // Trakt API Configuration
                { edit_field_bool!(trakt_form, translate.t(LABEL_ENABLED), enabled, TraktConfigFormAction::Enabled) }
                <div class="tp__form-section">
                    <h3>{translate.t(LABEL_API_CONFIGURATION)}</h3>
                    { edit_field_text!(trakt_api_form, translate.t(LABEL_TRAKT_API_KEY), api_key, TraktApiConfigFormAction::ApiKey) }
                    { edit_field_text!(trakt_api_form, translate.t(LABEL_TRAKT_API_VERSION), version, TraktApiConfigFormAction::Version) }
                    { edit_field_text!(trakt_api_form, translate.t(LABEL_TRAKT_API_URL), url, TraktApiConfigFormAction::Url) }
                    { edit_field_text!(trakt_api_form, translate.t(LABEL_USER_AGENT), user_agent, TraktApiConfigFormAction::UserAgent) }
                </div>

                // Trakt Lists
                { config_field_child!(translate.t(LABEL_TRAKT_LISTS), {
                    let trakt_lists_list = trakt_lists.clone();
                    html! {
                        <div class="tp__form-list">
                            <div class="tp__form-list__items">
                            {
                                for (*trakt_lists_list).iter().enumerate().map(|(idx, list)| {
                                    let content_type_str = match list.content_type {
                                        TraktContentType::Vod => "Vod",
                                        TraktContentType::Series => "Series",
                                        TraktContentType::Both => "Both",
                                    };
                                    html! {
                                        <div class="tp__form-list__item" key={format!("trakt-{idx}")}>
                                            <IconButton
                                                name={idx.to_string()}
                                                icon="Delete"
                                                onclick={handle_remove_trakt_list_item.clone()}/>
                                            <div class="tp__form-list__item-content">
                                                <span>
                                                    <strong>{&list.user}</strong>
                                                    {" / "}
                                                    {&list.list_slug}
                                                    {" - "}
                                                    {&list.category_name}
                                                    {" ("}
                                                    {content_type_str}
                                                    {", "}
                                                    {list.fuzzy_match_threshold}
                                                    {"%)"}
                                                </span>
                                            </div>
                                        </div>
                                    }
                                })
                            }
                            </div>

                            <TextButton
                                class="primary"
                                name="add_trakt_list"
                                icon="Add"
                                title={translate.t(LABEL_ADD_TRAKT_LIST)}
                                onclick={handle_show_trakt_list_form}
                            />
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
                <Panel value={XtreamOutputFormPage::Main.to_string()} active={view_visible.to_string()}>
                {render_output()}
                </Panel>
                <Panel value={XtreamOutputFormPage::Trakt.to_string()} active={view_visible.to_string()}>
                {render_trakt()}
                </Panel>
            </div>
            </div>
        }
    };

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
        <div class="tp__source-editor-form__sidebar">
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
        let block_id = props.block_id;
        Callback::from(move |_| {
            let mut output = output_form_state.data().clone();

            // Handle Trakt configuration
            let trakt_lists = (*trakt_lists_state).clone();
            output.trakt = if trakt_lists.is_empty() {
                None
            } else {
                Some(TraktConfigDto {
                    enabled: trakt_state.data().enabled,
                    api: trakt_api_state.data().clone(),
                    lists: trakt_lists,
                })
            };

            source_editor_ctx.on_form_change.emit((block_id, BlockInstance::Output(Rc::new(TargetOutputDto::Xtream(output)))));
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
             <TextButton class="secondary" name="cancel_input"
                icon="Cancel"
                title={ translate.t("LABEL.CANCEL")}
                onclick={handle_cancel}></TextButton>
             <TextButton class="primary" name="apply_input"
                icon="Accept"
                title={ translate.t("LABEL.OK")}
                onclick={handle_apply_target}></TextButton>
          </div>
          <div class="tp__source-editor-form__content">
                { render_sidebar() }
                { render_edit_mode() }
          </div>
        </div>
    }
}
