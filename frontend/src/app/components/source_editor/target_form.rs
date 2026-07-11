use crate::{
    app::components::{
        config::HasFormData, select::Select, BlockId, BlockInstance, Card, ClusterFlagsInput, ClusterFlagsInputMode,
        DropDownOption, DropDownSelection, EditMode, FilterInput, IconButton, Panel, SourceEditorContext, TextButton,
        ToggleSwitch,
    },
    config_field, config_field_bool, config_field_child, config_field_custom, edit_field_bool, edit_field_list_option,
    edit_field_text, generate_form_reducer,
    i18n::use_translation,
};
use shared::{
    error::TuliproxError,
    model::{ClusterFlags, ConfigTargetDto, ConfigTargetOptions, ConfigTargetShareLiveStreams, ProcessingOrder},
    utils::Internable,
};
use std::{fmt::Display, rc::Rc, str::FromStr, sync::Arc};
use yew::{
    component, html, use_context, use_effect_with, use_memo, use_reducer, use_state, Callback, Html, Properties,
    UseReducerHandle,
};

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
const LABEL_HLS: &str = "LABEL.HLS";
const LABEL_MPEG_TS: &str = "LABEL.MPEG_TS";
const LABEL_REMOVE_DUPLICATES: &str = "LABEL.REMOVE_DUPLICATES";
const LABEL_FORCE_REDIRECT: &str = "LABEL.FORCE_REDIRECT";
const LABEL_EPG_OUTPUT: &str = "LABEL.EPG_OUTPUT";
const LABEL_LOWERCASE_EPG_IDS: &str = "LABEL.LOWERCASE_EPG_IDS";
const LABEL_LOWERCASE_XMLTV_DISPLAY_NAMES: &str = "LABEL.LOWERCASE_XMLTV_DISPLAY_NAMES";
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
            _ => Err(TuliproxError::Config(format!("Unknown target form page: {s}"))),
        }
    }
}

impl Display for TargetFormPage {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match *self {
            TargetFormPage::Main => write!(f, "{}", TargetFormPage::MAIN),
            TargetFormPage::Options => write!(f, "{}", TargetFormPage::OPTIONS),
        }
    }
}

impl Internable for TargetFormPage {
    fn intern(self) -> Arc<str> {
        match self {
            Self::Main => TargetFormPage::MAIN,
            Self::Options => TargetFormPage::OPTIONS,
        }
        .intern()
    }
}

// pub sort: Option<ConfigSortDto>,
// pub rename: Option<Vec<ConfigRenameDto>>,
// pub favourites: Option<Vec<ConfigFavouritesDto>>,

#[derive(Debug, Clone, PartialEq)]
pub struct ConfigTargetOptionsFormState {
    pub form: ConfigTargetOptions,
    modified: bool,
}

impl HasFormData for ConfigTargetOptionsFormState {
    type Data = ConfigTargetOptions;

    fn data(&self) -> &Self::Data { &self.form }

    fn modified(&self) -> bool { self.modified }
}

#[allow(clippy::large_enum_variant)]
#[derive(Clone)]
pub enum ConfigTargetOptionsFormAction {
    IgnoreLogo(bool),
    ShareLiveStreams(bool),
    ShareLiveStreamsHls(bool),
    ShareLiveStreamsMpegTs(bool),
    RemoveDuplicates(bool),
    LowercaseEpgIds(bool),
    LowercaseXmltvDisplayNames(bool),
    ForceRedirect(Option<ClusterFlags>),
    SetAll(ConfigTargetOptions),
}

impl yew::prelude::Reducible for ConfigTargetOptionsFormState {
    type Action = ConfigTargetOptionsFormAction;

    fn reduce(self: Rc<Self>, action: Self::Action) -> Rc<Self> {
        let mut form = self.form.clone();
        let modified;

        match action {
            ConfigTargetOptionsFormAction::IgnoreLogo(value) => {
                form.ignore_logo = value;
                modified = true;
            }
            ConfigTargetOptionsFormAction::ShareLiveStreams(value) => {
                form.share_live_streams = ConfigTargetShareLiveStreams { hls: value, mpeg_ts: value };
                modified = true;
            }
            ConfigTargetOptionsFormAction::ShareLiveStreamsHls(value) => {
                form.share_live_streams.hls = value;
                modified = true;
            }
            ConfigTargetOptionsFormAction::ShareLiveStreamsMpegTs(value) => {
                form.share_live_streams.mpeg_ts = value;
                modified = true;
            }
            ConfigTargetOptionsFormAction::RemoveDuplicates(value) => {
                form.remove_duplicates = value;
                modified = true;
            }
            ConfigTargetOptionsFormAction::LowercaseEpgIds(value) => {
                form.epg_output.lowercase_ids = value;
                modified = true;
            }
            ConfigTargetOptionsFormAction::LowercaseXmltvDisplayNames(value) => {
                form.epg_output.lowercase_xmltv_display_names = value;
                modified = true;
            }
            ConfigTargetOptionsFormAction::ForceRedirect(value) => {
                form.force_redirect = value;
                modified = true;
            }
            ConfigTargetOptionsFormAction::SetAll(value) => {
                form = value;
                modified = false;
            }
        }

        Self { form, modified }.into()
    }
}

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
    #[prop_or(true)]
    pub(crate) allow_write: bool,
}

#[component]
pub fn ConfigTargetView(props: &ConfigTargetViewProps) -> Html {
    let translate = use_translation();
    let source_editor_ctx = use_context::<SourceEditorContext>().expect("SourceEditorContext not found");

    let target_form_state: UseReducerHandle<ConfigTargetFormState> =
        use_reducer(|| ConfigTargetFormState { form: ConfigTargetDto::default(), modified: false });
    let target_options_state: UseReducerHandle<ConfigTargetOptionsFormState> =
        use_reducer(|| ConfigTargetOptionsFormState { form: ConfigTargetOptions::default(), modified: false });

    let view_visible = use_state(|| TargetFormPage::Main);

    let handle_menu_click = {
        let active_menu = view_visible.clone();
        Callback::from(move |(name, _): (String, _)| {
            if let Ok(view_type) = TargetFormPage::from_str(&name) {
                active_menu.set(view_type);
            }
        })
    };

    let processing_orders = use_memo((*target_form_state).clone(), |target_state: &ConfigTargetFormState| {
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
        .map(|t| DropDownOption { id: t.to_string(), label: html! { t.to_string() }, selected: *t == default_po })
        .collect::<Vec<DropDownOption>>()
    });

    {
        let target_form_state = target_form_state.clone();
        let target_options_state = target_options_state.clone();

        let config_target = props.target.clone();

        use_effect_with(config_target, move |cfg| {
            if let Some(target) = cfg {
                target_form_state.dispatch(ConfigTargetFormAction::SetAll(target.as_ref().clone()));
                target_options_state.dispatch(ConfigTargetOptionsFormAction::SetAll(
                    target.options.as_ref().map_or_else(ConfigTargetOptions::default, |d| d.clone()),
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
        let target_option_toggle =
            |label: String, field_id: &str, value: bool, readonly: bool, on_change: Callback<bool>| {
                html! {
                    <div class="tp__form-field tp__form-field__bool tp__target-options__toggle-row">
                        <ToggleSwitch value={value} readonly={readonly} on_change={on_change} />
                        <crate::app::components::FieldLabel
                            label={label}
                            field_id={field_id.to_string()}
                        />
                    </div>
                }
            };
        let render_epg_output =
            |readonly: bool, lowercase_ids_on_change: Callback<bool>, lowercase_names_on_change: Callback<bool>| {
                html! {
                    <div class="tp__target-options__group">
                        <div class="tp__target-options__heading">
                            <span class="tp__form-field__label">{ translate.t(LABEL_EPG_OUTPUT) }</span>
                        </div>
                        <div class="tp__target-options__children">
                            { target_option_toggle(
                                translate.t(LABEL_LOWERCASE_EPG_IDS),
                                "EPG_OUTPUT_OPTIONS.LOWERCASE_IDS",
                                target_options_state.form.epg_output.lowercase_ids,
                                readonly,
                                lowercase_ids_on_change,
                            ) }
                            { target_option_toggle(
                                translate.t(LABEL_LOWERCASE_XMLTV_DISPLAY_NAMES),
                                "EPG_OUTPUT_OPTIONS.LOWERCASE_XMLTV_DISPLAY_NAMES",
                                target_options_state.form.epg_output.lowercase_xmltv_display_names,
                                readonly,
                                lowercase_names_on_change,
                            ) }
                        </div>
                    </div>
                }
            };
        if !props.allow_write {
            html! {
                <Card class="tp__config-view__card">
                    <div class="tp__target-options">
                        { config_field_bool!(target_options_state.form, translate.t(LABEL_IGNORE_LOGO), ignore_logo) }
                        <div class="tp__target-options__group">
                            { target_option_toggle(
                                translate.t(LABEL_SHARE_LIVE_STREAMS),
                                "CONFIG_TARGET_OPTIONS.SHARE_LIVE_STREAMS",
                                target_options_state.form.share_live_any_enabled(),
                                true,
                                Callback::noop(),
                            ) }
                            <div class="tp__target-options__children">
                                { target_option_toggle(
                                    translate.t(LABEL_HLS),
                                    "CONFIG_TARGET_SHARE_LIVE_STREAMS.HLS",
                                    target_options_state.form.share_live_hls_enabled(),
                                    true,
                                    Callback::noop(),
                                ) }
                                { target_option_toggle(
                                    translate.t(LABEL_MPEG_TS),
                                    "CONFIG_TARGET_SHARE_LIVE_STREAMS.MPEG_TS",
                                    target_options_state.form.share_live_mpeg_ts_enabled(),
                                    true,
                                    Callback::noop(),
                                ) }
                            </div>
                        </div>
                        { config_field_bool!(target_options_state.form, translate.t(LABEL_REMOVE_DUPLICATES), remove_duplicates) }
                        { render_epg_output(true, Callback::noop(), Callback::noop()) }
                        <div class="tp__target-options__group">
                            <div class="tp__target-options__heading">
                                <span class="tp__form-field__label">{ translate.t(LABEL_FORCE_REDIRECT) }</span>
                            </div>
                            <div class="tp__target-options__children">
                                <div class="tp__target-options__child-content">
                                    <span class="tp__form-field__value">
                                        { target_options_state.form.force_redirect.map_or_else(String::new, |flags| flags.to_string()) }
                                    </span>
                                </div>
                            </div>
                        </div>
                    </div>
                </Card>
            }
        } else {
            let share_live_on_change = {
                let target_options_state = target_options_state.clone();
                Callback::from(move |value| {
                    target_options_state.dispatch(ConfigTargetOptionsFormAction::ShareLiveStreams(value));
                })
            };
            let share_live_hls_on_change = {
                let target_options_state = target_options_state.clone();
                Callback::from(move |value| {
                    target_options_state.dispatch(ConfigTargetOptionsFormAction::ShareLiveStreamsHls(value));
                })
            };
            let share_live_mpeg_ts_on_change = {
                let target_options_state = target_options_state.clone();
                Callback::from(move |value| {
                    target_options_state.dispatch(ConfigTargetOptionsFormAction::ShareLiveStreamsMpegTs(value));
                })
            };
            let lowercase_epg_ids_on_change = {
                let target_options_state = target_options_state.clone();
                Callback::from(move |value| {
                    target_options_state.dispatch(ConfigTargetOptionsFormAction::LowercaseEpgIds(value));
                })
            };
            let lowercase_xmltv_display_names_on_change = {
                let target_options_state = target_options_state.clone();
                Callback::from(move |value| {
                    target_options_state.dispatch(ConfigTargetOptionsFormAction::LowercaseXmltvDisplayNames(value));
                })
            };
            html! {
                <Card class="tp__config-view__card">
                <div class="tp__target-options">
                    { edit_field_bool!(target_options_state, translate.t(LABEL_IGNORE_LOGO), ignore_logo,  ConfigTargetOptionsFormAction::IgnoreLogo) }
                    <div class="tp__target-options__group">
                        { target_option_toggle(
                            translate.t(LABEL_SHARE_LIVE_STREAMS),
                            "CONFIG_TARGET_OPTIONS.SHARE_LIVE_STREAMS",
                            target_options_state.form.share_live_any_enabled(),
                            false,
                            share_live_on_change,
                        ) }
                        <div class="tp__target-options__children">
                            { target_option_toggle(
                                translate.t(LABEL_HLS),
                                "CONFIG_TARGET_SHARE_LIVE_STREAMS.HLS",
                                target_options_state.form.share_live_hls_enabled(),
                                false,
                                share_live_hls_on_change,
                            ) }
                            { target_option_toggle(
                                translate.t(LABEL_MPEG_TS),
                                "CONFIG_TARGET_SHARE_LIVE_STREAMS.MPEG_TS",
                                target_options_state.form.share_live_mpeg_ts_enabled(),
                                false,
                                share_live_mpeg_ts_on_change,
                            ) }
                        </div>
                    </div>
                    { edit_field_bool!(target_options_state, translate.t(LABEL_REMOVE_DUPLICATES), remove_duplicates, ConfigTargetOptionsFormAction::RemoveDuplicates) }
                    { render_epg_output(
                        false,
                        lowercase_epg_ids_on_change,
                        lowercase_xmltv_display_names_on_change,
                    ) }
                    <div class="tp__target-options__group">
                        <div class="tp__target-options__heading">
                            <span class="tp__form-field__label">{ translate.t(LABEL_FORCE_REDIRECT) }</span>
                        </div>
                        <div class="tp__target-options__children">
                            <div class="tp__target-options__child-content">
                                <ClusterFlagsInput
                                    name="force_redirect"
                                    value={target_options_state.form.force_redirect}
                                    mode={ClusterFlagsInputMode::NoneIsNone}
                                    on_change={Callback::from(move |(_name, flags):(String, Option<ClusterFlags>)| {
                                        target_options_state_1.dispatch(ConfigTargetOptionsFormAction::ForceRedirect(flags));
                                    })}
                                />
                            </div>
                        </div>
                    </div>
                </div>
                </Card>
            }
        }
    };

    let render_target = || {
        let target_form_state_1 = target_form_state.clone();
        let target_form_state_2 = target_form_state.clone();
        if !props.allow_write {
            html! {
                <Card class="tp__config-view__card">
                    <div class="tp__config-view__cols-2">
                        { config_field_bool!(target_form_state.form, translate.t(LABEL_ENABLED), enabled) }
                        { config_field_bool!(target_form_state.form, translate.t(LABEL_USE_MEMORY_CACHE), use_memory_cache) }
                    </div>
                    { config_field!(target_form_state.form, translate.t(LABEL_NAME), name) }
                    { config_field_custom!(translate.t(LABEL_FILTER), target_form_state.form.filter.clone()) }
                    { config_field_custom!(
                        translate.t(LABEL_PROCESSING_ORDER),
                        target_form_state.form.processing_order.to_string()
                    ) }
                    { config_field_custom!(
                        translate.t(LABEL_MAPPING),
                        target_form_state.form.mapping.as_ref().map_or_else(String::new, |values| values.join(", "))
                    ) }
                    { config_field_custom!(
                        translate.t(LABEL_WATCH),
                        target_form_state.form.watch.as_ref().map_or_else(String::new, |values| values.join(", "))
                    ) }
                </Card>
            }
        } else {
            html! {
                <Card class="tp__config-view__card">
                <div class="tp__config-view__cols-2">
                { edit_field_bool!(target_form_state, translate.t(LABEL_ENABLED), enabled,  ConfigTargetFormAction::Enabled) }
                { edit_field_bool!(target_form_state, translate.t(LABEL_USE_MEMORY_CACHE), use_memory_cache,  ConfigTargetFormAction::UseMemoryCache) }
                </div>
                { edit_field_text!(target_form_state, translate.t(LABEL_NAME), name, ConfigTargetFormAction::Name) }
                { config_field_child!(translate.t(LABEL_FILTER), "TARGET_FORM.FILTER", {
                       html! {
                            <FilterInput filter={target_form_state_2.form.filter.clone()} on_change={Callback::from(move |new_filter: Option<String>| {
                                target_form_state_2.dispatch(ConfigTargetFormAction::Filter(new_filter.unwrap_or_default()));
                            })} />
                       }
                })}

                { config_field_child!(translate.t(LABEL_PROCESSING_ORDER), "TARGET_FORM.PROCESSING_ORDER", {
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
        }
    };

    let render_edit_mode = || {
        html! {
            <div class="tp__source-editor-form__body">
            <div class="tp__source-editor-form__body__pages">
                <Panel value={TargetFormPage::Main.intern()} active={view_visible.intern()}>
                {render_target()}
                </Panel>
                <Panel value={TargetFormPage::Options.intern()} active={view_visible.intern()}>
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
         if props.allow_write {
             <TextButton class="primary" name="apply_input"
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
    use super::{ConfigTargetOptionsFormAction, ConfigTargetOptionsFormState};
    use shared::model::ConfigTargetOptions;
    use std::rc::Rc;
    use yew::prelude::Reducible;

    fn default_options_state() -> Rc<ConfigTargetOptionsFormState> {
        ConfigTargetOptionsFormState { form: ConfigTargetOptions::default(), modified: false }.into()
    }

    #[test]
    fn lowercase_epg_ids_action_updates_nested_option() {
        let state = default_options_state().reduce(ConfigTargetOptionsFormAction::LowercaseEpgIds(true));

        assert!(state.form.epg_output.lowercase_ids);
        assert!(state.modified);
        assert!(!state.form.epg_output.lowercase_xmltv_display_names);
    }

    #[test]
    fn lowercase_xmltv_display_names_action_updates_nested_option() {
        let state = default_options_state().reduce(ConfigTargetOptionsFormAction::LowercaseXmltvDisplayNames(true));

        assert!(state.form.epg_output.lowercase_xmltv_display_names);
        assert!(state.modified);
        assert!(!state.form.epg_output.lowercase_ids);
    }
}
