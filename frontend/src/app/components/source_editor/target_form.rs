use crate::app::components::config::HasFormData;
use crate::app::components::select::Select;
use crate::app::components::{BlockId, BlockInstance, Card, ClusterFlagsInput, ClusterFlagsInputMode, DropDownOption, DropDownSelection, EditMode, FilterInput, IconButton, Panel, SourceEditorContext, TextButton};
use crate::{config_field_child, edit_field_bool, edit_field_list_option, edit_field_text, generate_form_reducer};
use shared::model::{ClusterFlags, ConfigTargetDto, ConfigTargetOptions, ProcessingOrder};
use std::fmt::Display;
use std::rc::Rc;
use std::str::FromStr;
use yew::{function_component, html, use_context, use_effect_with, use_memo, use_reducer, use_state, Callback, Html, Properties, UseReducerHandle};
use yew_i18n::use_translation;
use shared::error::TuliproxError;
use shared::info_err_res;

const LABEL_ENABLED: &str = "LABEL.ENABLED";
const LABEL_NAME: &str = "LABEL.NAME";
const LABEL_FILTER: &str = "LABEL.FILTER";
const LABEL_MAPPING: &str = "LABEL.MAPPING";
const LABEL_WATCH: &str = "LABEL.WATCH";
const LABEL_ADD_MAPPING: &str = "LABEL.ADD_MAPPING";
const LABEL_ADD_WATCH: &str = "LABEL.ADD_WATCH";
const LABEL_USE_MEMORY_CACHE: &str = "LABEL.USE_MEMORY_CACHE";
const LABEL_PROCESSING_ORDER: &str = "LABEL.PROCESSING_ORDER";
const LABEL_IGNORE_LOGO: &str = "LABEL.IGNORE_LOGO";
const LABEL_SHARE_LIVE_STREAMS: &str = "LABEL.SHARE_LIVE_STREAMS";
const LABEL_REMOVE_DUPLICATES: &str = "LABEL.REMOVE_DUPLICATES";
const LABEL_FORCE_REDIRECT: &str = "LABEL.FORCE_REDIRECT";
const LABEL_MAIN: &str = "LABEL.MAIN_CONFIG";
const LABEL_OPTIONS: &str = "LABEL.OPTIONS";

#[derive(Copy, Clone, PartialEq, Eq)]
enum TargetFormPage {
    Main,
    Options,
}

impl TargetFormPage {
    const MAIN: &str = "Main";
    const OPTIONS: &str = "Options";
}

impl FromStr for TargetFormPage {
    type Err = TuliproxError;

    fn from_str(s: &str) -> Result<Self, TuliproxError> {
        match s {
            Self::MAIN => Ok(TargetFormPage::Main),
            Self::OPTIONS => Ok(TargetFormPage::Options),
            _ => info_err_res!("Unknown target form page: {s}"),
        }
    }
}

impl Display for TargetFormPage {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match *self {
            TargetFormPage::Main => write!(f, "Main"),
            TargetFormPage::Options => write!(f, "Options"),
        }
    }
}


// pub sort: Option<ConfigSortDto>,
// pub rename: Option<Vec<ConfigRenameDto>>,
// pub favourites: Option<Vec<ConfigFavouritesDto>>,

generate_form_reducer!(
    state: ConfigTargetOptionsFormState { form: ConfigTargetOptions },
    action_name: ConfigTargetOptionsFormAction,
    fields {
        IgnoreLogo => ignore_logo: bool,
        ShareLiveStreams => share_live_streams: bool,
        RemoveDuplicates => remove_duplicates: bool,
        ForceRedirect => force_redirect: Option<ClusterFlags>,
    }
);

generate_form_reducer!(
    state: ConfigTargetFormState { form: ConfigTargetDto },
    action_name: ConfigTargetFormAction,
    fields {
        Enabled => enabled: bool,
        Name => name: String,
        ProcessingOrder => processing_order: ProcessingOrder,
        Filter => filter: String,
        Mapping => mapping: Option<Vec<String>>,
        Watch => watch: Option<Vec<String>>,
        UseMemoryCache => use_memory_cache: bool,
    }
);

#[derive(Properties, PartialEq, Clone)]
pub struct ConfigTargetViewProps {
    pub(crate) block_id: BlockId,
    pub(crate) target: Option<Rc<ConfigTargetDto>>,
}

#[function_component]
pub fn ConfigTargetView(props: &ConfigTargetViewProps) -> Html {
    let translate = use_translation();
    let source_editor_ctx = use_context::<SourceEditorContext>().expect("SourceEditorContext not found");

    let target_form_state: UseReducerHandle<ConfigTargetFormState> =
        use_reducer(|| ConfigTargetFormState {
            form: ConfigTargetDto::default(),
            modified: false,
        });
    let target_options_state: UseReducerHandle<ConfigTargetOptionsFormState> =
        use_reducer(|| ConfigTargetOptionsFormState {
            form: ConfigTargetOptions::default(),
            modified: false,
        });

    let view_visible = use_state(|| TargetFormPage::Main);


    let handle_menu_click = {
        let active_menu = view_visible.clone();
        Callback::from(move |(name, _): (String, _)| {
            if let Ok(view_type) = TargetFormPage::from_str(&name) {
                active_menu.set(view_type);
            }
        })
    };


    let processing_orders = use_memo(target_form_state.clone(), |target_state: &UseReducerHandle<ConfigTargetFormState>| {
        let default_po = target_state.form.processing_order;
        [
            ProcessingOrder::Frm,
            ProcessingOrder::Fmr,
            ProcessingOrder::Rfm,
            ProcessingOrder::Rmf,
            ProcessingOrder::Mfr,
            ProcessingOrder::Mrf,
        ]
            .iter()
            .map(|t| DropDownOption {
                id: t.to_string(),
                label: html! { t.to_string() },
                selected: *t == default_po,
            }).collect::<Vec<DropDownOption>>()
    });

    {
        let target_form_state = target_form_state.clone();
        let target_options_state = target_options_state.clone();

        let config_target = props.target.clone();

        use_effect_with(config_target, move |cfg| {
            if let Some(target) = cfg {
                target_form_state.dispatch(ConfigTargetFormAction::SetAll(target.as_ref().clone()));
                target_options_state.dispatch(ConfigTargetOptionsFormAction::SetAll(
                    target.options.as_ref()
                        .map_or_else(ConfigTargetOptions::default, |d| d.clone()),
                ));
            } else {
                target_form_state.dispatch(ConfigTargetFormAction::SetAll(ConfigTargetDto::default()));
                target_options_state.dispatch(ConfigTargetOptionsFormAction::SetAll(ConfigTargetOptions::default()));
            }
            || ()
        });
    }

    let render_options = || {
        let target_options_state_1 = target_options_state.clone();
        html! {
            <Card class="tp__config-view__card">
            <div class="tp__config-view__cols-2">
            { edit_field_bool!(target_options_state, translate.t(LABEL_IGNORE_LOGO), ignore_logo,  ConfigTargetOptionsFormAction::IgnoreLogo) }
            { edit_field_bool!(target_options_state, translate.t(LABEL_SHARE_LIVE_STREAMS), share_live_streams, ConfigTargetOptionsFormAction::ShareLiveStreams) }
            </div>
            { edit_field_bool!(target_options_state, translate.t(LABEL_REMOVE_DUPLICATES), remove_duplicates, ConfigTargetOptionsFormAction::RemoveDuplicates) }
            { config_field_child!(translate.t(LABEL_FORCE_REDIRECT), {
               html! {
                    <ClusterFlagsInput
                        name="force_redirect"
                        value={target_options_state.form.force_redirect}
                        mode={ClusterFlagsInputMode::NoneIsNone}
                        on_change={Callback::from(move |(_name, flags):(String, Option<ClusterFlags>)| {
                        target_options_state_1.dispatch(ConfigTargetOptionsFormAction::ForceRedirect(flags));
                    })}
                />
            }})}
            </Card>
        }
    };

    let render_target = || {
        let target_form_state_1 = target_form_state.clone();
        let target_form_state_2 = target_form_state.clone();
        html! {
            <Card class="tp__config-view__card">
            <div class="tp__config-view__cols-2">
            { edit_field_bool!(target_form_state, translate.t(LABEL_ENABLED), enabled,  ConfigTargetFormAction::Enabled) }
            { edit_field_bool!(target_form_state, translate.t(LABEL_USE_MEMORY_CACHE), use_memory_cache,  ConfigTargetFormAction::UseMemoryCache) }
            </div>
            { edit_field_text!(target_form_state, translate.t(LABEL_NAME), name, ConfigTargetFormAction::Name) }
            { config_field_child!(translate.t(LABEL_FILTER), {
                   html! {
                        <FilterInput filter={target_form_state_2.form.filter.clone()} on_change={Callback::from(move |new_filter: Option<String>| {
                            target_form_state_2.dispatch(ConfigTargetFormAction::Filter(new_filter.unwrap_or_default()));
                        })} />
                   }
            })}

            { config_field_child!(translate.t(LABEL_PROCESSING_ORDER), {
                   html! {
                       <Select
                        name={"processing_order"}
                        multi_select={false}
                        on_select={Callback::from(move |(_, selections):(String, DropDownSelection)| {
                           match selections {
                            DropDownSelection::Empty => {
                                   target_form_state_1.dispatch(ConfigTargetFormAction::ProcessingOrder(ProcessingOrder::Frm));
                            }
                            DropDownSelection::Single(option) => {
                                target_form_state_1.dispatch(ConfigTargetFormAction::ProcessingOrder(option.parse::<ProcessingOrder>().unwrap_or(ProcessingOrder::Frm)));
                            }
                            DropDownSelection::Multi(options) => {
                              if let Some(first) = options.first() {
                                target_form_state_1.dispatch(ConfigTargetFormAction::ProcessingOrder(first.parse::<ProcessingOrder>().unwrap_or(ProcessingOrder::Frm)));
                               }
                             }
                           }
                        })}
                        options={processing_orders.clone()}
                    />
               }})}
            { edit_field_list_option!(target_form_state, translate.t(LABEL_MAPPING), mapping, ConfigTargetFormAction::Mapping, translate.t(LABEL_ADD_MAPPING)) }
            { edit_field_list_option!(target_form_state, translate.t(LABEL_WATCH), watch, ConfigTargetFormAction::Watch, translate.t(LABEL_ADD_WATCH)) }
            </Card>
        }
    };

    let render_edit_mode = || {
        html! {
            <div class="tp__source-editor-form__body">
            <div class="tp__source-editor-form__body__pages">
                <Panel value={TargetFormPage::Main.to_string()} active={view_visible.to_string()}>
                {render_target()}
                </Panel>
                <Panel value={TargetFormPage::Options.to_string()} active={view_visible.to_string()}>
                {render_options()}
                </Panel>
            </div>
            </div>
        }
    };

    let render_sidebar = || {
        html! {
            <div class="tp__source-editor-form__sidebar">
            <IconButton class={format!("tp__app-sidebar-menu--{}{}", TargetFormPage::Main, if *view_visible == TargetFormPage::Main { " active" } else {""})}  icon="Settings" hint={translate.t(LABEL_MAIN)} name={TargetFormPage::Main.to_string()} onclick={&handle_menu_click}></IconButton>
            <IconButton class={format!("tp__app-sidebar-menu--{}{}", TargetFormPage::Options, if *view_visible == TargetFormPage::Options { " active" } else {""})}  icon="Options" hint={translate.t(LABEL_OPTIONS)} name={TargetFormPage::Options.to_string()} onclick={&handle_menu_click}></IconButton>
          </div>
        }
    };

    let handle_apply_target = {
        let source_editor_ctx = source_editor_ctx.clone();
        let target_form_state = target_form_state.clone();
        let target_options_state = target_options_state.clone();
        let block_id = props.block_id;
        Callback::from(move |_| {
            let mut target = target_form_state.data().clone();
            let target_options = target_options_state.data();
            if !target_options.is_empty() {
                target.options = Some(target_options.clone());
            } else {
                target.options = None;
            }
            source_editor_ctx.on_form_change.emit((block_id, BlockInstance::Target(Rc::new(target))));
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
             <div class="tp__source-editor-form_toolbar tp__form-page__toolbar">
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
