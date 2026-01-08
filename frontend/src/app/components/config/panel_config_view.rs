use crate::app::components::input::Input;
use crate::app::components::{Card, Chip, IconButton, ToggleSwitch};
use crate::app::components::config::config_page::{ConfigForm, LABEL_PANEL_CONFIG};
use crate::app::components::config::config_view_context::ConfigViewContext;
use crate::app::context::ConfigContext;
use crate::html_if;
use shared::model::{ConfigInputDto, PanelApiConfigDto, PanelApiQueryParamDto, SourcesConfigDto};
use std::rc::Rc;
use yew::prelude::*;
use yew_i18n::use_translation;

const LABEL_ENABLED: &str = "LABEL.ENABLED";
const LABEL_URL: &str = "LABEL.URL";
const LABEL_API_KEY: &str = "LABEL.API_KEY";
const LABEL_STATUS_READY: &str = "LABEL.PANEL_STATUS_READY";
const LABEL_STATUS_INVALID: &str = "LABEL.PANEL_STATUS_INVALID";
const LABEL_STATUS_DISABLED: &str = "LABEL.PANEL_STATUS_DISABLED";
const LABEL_VALIDATION: &str = "LABEL.VALIDATION";
const HINT_PANEL_INFO: &str = "HINT.CONFIG.PANEL.INFO";
const HINT_PANEL_ENABLE: &str = "HINT.CONFIG.PANEL.ENABLE";

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum PanelSection {
    Info,
    New,
    Renew,
}

impl PanelSection {
    fn label_key(&self) -> &'static str {
        match self {
            Self::Info => "LABEL.PANEL_CLIENT_INFO",
            Self::New => "LABEL.PANEL_CLIENT_NEW",
            Self::Renew => "LABEL.PANEL_CLIENT_RENEW",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
struct PanelConfigFormState {
    form: SourcesConfigDto,
    modified: bool,
}

#[allow(clippy::large_enum_variant)]
#[derive(Clone)]
enum PanelConfigFormAction {
    SetAll(SourcesConfigDto),
    SetEnabled { input_idx: usize, enabled: bool },
    SetPanelUrl { input_idx: usize, url: String },
    SetApiKey { input_idx: usize, api_key: String },
    AddParam { input_idx: usize, section: PanelSection },
    RemoveParam { input_idx: usize, section: PanelSection, param_idx: usize },
    SetParamKey { input_idx: usize, section: PanelSection, param_idx: usize, key: String },
    SetParamValue { input_idx: usize, section: PanelSection, param_idx: usize, value: String },
    EnsureRequired { input_idx: usize, section: PanelSection },
}

fn params_mut(panel: &mut PanelApiConfigDto, section: PanelSection) -> &mut Vec<PanelApiQueryParamDto> {
    match section {
        PanelSection::Info => &mut panel.query_parameter.client_info,
        PanelSection::New => &mut panel.query_parameter.client_new,
        PanelSection::Renew => &mut panel.query_parameter.client_renew,
    }
}

fn has_param(params: &[PanelApiQueryParamDto], key: &str) -> bool {
    params.iter().any(|p| p.key.trim().eq_ignore_ascii_case(key))
}

fn get_param_value(params: &[PanelApiQueryParamDto], key: &str) -> Option<String> {
    params
        .iter()
        .find(|p| p.key.trim().eq_ignore_ascii_case(key))
        .map(|p| p.value.trim().to_string())
}

fn validate_panel(panel: Option<&PanelApiConfigDto>) -> Vec<String> {
    let mut errors = vec![];
    let Some(panel) = panel else {
        return errors;
    };

    if panel.url.trim().is_empty() {
        errors.push("Missing panel url".to_string());
    }
    if panel.api_key.as_ref().is_none_or(|k| k.trim().is_empty()) {
        errors.push("Missing api_key".to_string());
    }

    let sections = [
        (PanelSection::Info, &panel.query_parameter.client_info),
        (PanelSection::New, &panel.query_parameter.client_new),
        (PanelSection::Renew, &panel.query_parameter.client_renew),
    ];
    for (section, params) in sections {
        if params.is_empty() {
            errors.push(match section {
                PanelSection::Info => "client_info: empty".to_string(),
                PanelSection::New => "client_new: empty".to_string(),
                PanelSection::Renew => "client_renew: empty".to_string(),
            });
            continue;
        }
        if !has_param(params, "api_key") {
            errors.push(match section {
                PanelSection::Info => "client_info: missing api_key param".to_string(),
                PanelSection::New => "client_new: missing api_key param".to_string(),
                PanelSection::Renew => "client_renew: missing api_key param".to_string(),
            });
        }
        match section {
            PanelSection::New => {
                if has_param(params, "user") {
                    errors.push("client_new: must not include user".to_string());
                }
                if get_param_value(params, "type").as_deref() != Some("m3u") {
                    errors.push("client_new: type must be m3u".to_string());
                }
            }
            PanelSection::Renew => {
                if get_param_value(params, "type").as_deref() != Some("m3u") {
                    errors.push("client_renew: type must be m3u".to_string());
                }
                if get_param_value(params, "username").as_deref() != Some("auto") {
                    errors.push("client_renew: username must be auto".to_string());
                }
                if get_param_value(params, "password").as_deref() != Some("auto") {
                    errors.push("client_renew: password must be auto".to_string());
                }
            }
            PanelSection::Info => {
                if get_param_value(params, "username").as_deref() != Some("auto") {
                    errors.push("client_info: username must be auto".to_string());
                }
                if get_param_value(params, "password").as_deref() != Some("auto") {
                    errors.push("client_info: password must be auto".to_string());
                }
            }
        }
    }
    errors
}

fn ensure_required_params(params: &mut Vec<PanelApiQueryParamDto>, section: PanelSection) {
    let ensure = |params: &mut Vec<PanelApiQueryParamDto>, key: &str, value: &str| {
        if !has_param(params, key) {
            params.push(PanelApiQueryParamDto {
                key: key.to_string(),
                value: value.to_string(),
            });
        }
    };
    ensure(params, "api_key", "auto");
    match section {
        PanelSection::New => {
            params.retain(|p| !p.key.trim().eq_ignore_ascii_case("user"));
            ensure(params, "type", "m3u");
        }
        PanelSection::Renew => {
            ensure(params, "type", "m3u");
            ensure(params, "username", "auto");
            ensure(params, "password", "auto");
        }
        PanelSection::Info => {
            ensure(params, "username", "auto");
            ensure(params, "password", "auto");
        }
    }
}

fn with_input_mut(form: &mut SourcesConfigDto, input_idx: usize, f: impl FnOnce(&mut ConfigInputDto)) {
    if let Some(input) = form.inputs.get_mut(input_idx) {
        f(input);
    }
}

impl Reducible for PanelConfigFormState {
    type Action = PanelConfigFormAction;

    fn reduce(self: Rc<Self>, action: Self::Action) -> Rc<Self> {
        match action {
            PanelConfigFormAction::SetAll(form) => Self { form, modified: false }.into(),
            PanelConfigFormAction::SetEnabled { input_idx, enabled } => {
                let mut form = self.form.clone();
                with_input_mut(&mut form, input_idx, |input| {
                    if enabled {
                        if input.panel_api.is_none() {
                            input.panel_api = Some(PanelApiConfigDto::default());
                        }
                    } else {
                        input.panel_api = None;
                    }
                });
                Self { form, modified: true }.into()
            }
            PanelConfigFormAction::SetPanelUrl { input_idx, url } => {
                let mut form = self.form.clone();
                with_input_mut(&mut form, input_idx, |input| {
                    let panel = input.panel_api.get_or_insert_with(PanelApiConfigDto::default);
                    panel.url = url;
                });
                Self { form, modified: true }.into()
            }
            PanelConfigFormAction::SetApiKey { input_idx, api_key } => {
                let mut form = self.form.clone();
                with_input_mut(&mut form, input_idx, |input| {
                    let panel = input.panel_api.get_or_insert_with(PanelApiConfigDto::default);
                    panel.api_key = if api_key.trim().is_empty() { None } else { Some(api_key) };
                });
                Self { form, modified: true }.into()
            }
            PanelConfigFormAction::AddParam { input_idx, section } => {
                let mut form = self.form.clone();
                with_input_mut(&mut form, input_idx, |input| {
                    let panel = input.panel_api.get_or_insert_with(PanelApiConfigDto::default);
                    params_mut(panel, section).push(PanelApiQueryParamDto { key: String::new(), value: String::new() });
                });
                Self { form, modified: true }.into()
            }
            PanelConfigFormAction::RemoveParam { input_idx, section, param_idx } => {
                let mut form = self.form.clone();
                with_input_mut(&mut form, input_idx, |input| {
                    if let Some(panel) = input.panel_api.as_mut() {
                        let params = params_mut(panel, section);
                        if param_idx < params.len() {
                            params.remove(param_idx);
                        }
                    }
                });
                Self { form, modified: true }.into()
            }
            PanelConfigFormAction::SetParamKey { input_idx, section, param_idx, key } => {
                let mut form = self.form.clone();
                with_input_mut(&mut form, input_idx, |input| {
                    if let Some(panel) = input.panel_api.as_mut() {
                        let params = params_mut(panel, section);
                        if let Some(p) = params.get_mut(param_idx) {
                            p.key = key;
                        }
                    }
                });
                Self { form, modified: true }.into()
            }
            PanelConfigFormAction::SetParamValue { input_idx, section, param_idx, value } => {
                let mut form = self.form.clone();
                with_input_mut(&mut form, input_idx, |input| {
                    if let Some(panel) = input.panel_api.as_mut() {
                        let params = params_mut(panel, section);
                        if let Some(p) = params.get_mut(param_idx) {
                            p.value = value;
                        }
                    }
                });
                Self { form, modified: true }.into()
            }
            PanelConfigFormAction::EnsureRequired { input_idx, section } => {
                let mut form = self.form.clone();
                with_input_mut(&mut form, input_idx, |input| {
                    let panel = input.panel_api.get_or_insert_with(PanelApiConfigDto::default);
                    ensure_required_params(params_mut(panel, section), section);
                });
                Self { form, modified: true }.into()
            }
        }
    }
}

fn render_param_editor(
    form_state: &UseReducerHandle<PanelConfigFormState>,
    edit_mode: bool,
    input_idx: usize,
    section: PanelSection,
    section_title: String,
    params: &[PanelApiQueryParamDto],
) -> Html {
    let add_required = {
        let form_state = form_state.clone();
        Callback::from(move |(name, _): (String, web_sys::MouseEvent)| {
            if name == "required" {
                form_state.dispatch(PanelConfigFormAction::EnsureRequired { input_idx, section });
            }
        })
    };

    let add_param = {
        let form_state = form_state.clone();
        Callback::from(move |(name, _): (String, web_sys::MouseEvent)| {
            if name == "add" {
                form_state.dispatch(PanelConfigFormAction::AddParam { input_idx, section });
            }
        })
    };

    html! {
        <div class="tp__panel-config-view__section">
            <div class="tp__panel-config-view__section-header">
                <h2>{ section_title }</h2>
                { html_if!(edit_mode, {
                    <div class="tp__panel-config-view__section-actions">
                        <IconButton name="required" icon="Accept" class="secondary" onclick={add_required}/>
                        <IconButton name="add" icon="Add" class="primary" onclick={add_param}/>
                    </div>
                })}
            </div>
            <div class="tp__panel-config-view__params">
            {
                if params.is_empty() {
                    html!{ <div class="tp__panel-config-view__params-empty">{ "—" }</div> }
                } else {
                    html!{ for params.iter().enumerate().map(|(param_idx, p)| {
                        let on_key = {
                            let form_state = form_state.clone();
                            Callback::from(move |value: String| {
                                form_state.dispatch(PanelConfigFormAction::SetParamKey { input_idx, section, param_idx, key: value });
                            })
                        };
                        let on_val = {
                            let form_state = form_state.clone();
                            Callback::from(move |value: String| {
                                form_state.dispatch(PanelConfigFormAction::SetParamValue { input_idx, section, param_idx, value });
                            })
                        };
                        let on_remove = {
                            let form_state = form_state.clone();
                            Callback::from(move |(name, _): (String, web_sys::MouseEvent)| {
                                if name == "rm" {
                                    form_state.dispatch(PanelConfigFormAction::RemoveParam { input_idx, section, param_idx });
                                }
                            })
                        };
                        html!{
                            <div class={classes!(
                                "tp__panel-config-view__param-row",
                                if edit_mode { None } else { Some("tp__panel-config-view__param-row--view") }
                            )}>
                                {
                                    if edit_mode {
                                        html!{
                                            <>
                                                <Input name="key" label={Option::<String>::None} value={p.key.clone()} placeholder={Some("key".to_string())} on_change={Some(on_key)} />
                                                <Input name="value" label={Option::<String>::None} value={p.value.clone()} placeholder={Some("value".to_string())} on_change={Some(on_val)} />
                                                <IconButton name="rm" icon="Delete" class="tp__panel-config-view__param-remove" onclick={on_remove}/>
                                            </>
                                        }
                                    } else {
                                        html!{
                                            <div class="tp__panel-config-view__param-view">
                                                <span class="k">{ &p.key }</span>
                                                <span class="v">{ &p.value }</span>
                                            </div>
                                        }
                                    }
                                }
                            </div>
                        }
                    }) }
                }
            }
            </div>
        </div>
    }
}

#[function_component]
pub fn PanelConfigView() -> Html {
    let translate = use_translation();
    let config_ctx = use_context::<ConfigContext>().expect("ConfigContext not found");
    let config_view_ctx = use_context::<ConfigViewContext>().expect("ConfigViewContext not found");

    let form_state: UseReducerHandle<PanelConfigFormState> = use_reducer(|| PanelConfigFormState {
        form: SourcesConfigDto::default(),
        modified: false,
    });

    {
        let on_form_change = config_view_ctx.on_form_change.clone();
        let deps = (form_state.clone(), form_state.modified);
        use_effect_with(deps, move |(state, modified)| {
            on_form_change.emit(ConfigForm::Panel(*modified, state.form.clone()));
        });
    }

    {
        let form_state = form_state.clone();
        let sources_cfg = config_ctx.config.as_ref().map(|c| c.sources.clone());
        use_effect_with((sources_cfg, config_view_ctx.edit_mode.clone()), move |(sources_cfg, _mode)| {
            if let Some(src) = sources_cfg {
                form_state.dispatch(PanelConfigFormAction::SetAll((*src).clone()));
            } else {
                form_state.dispatch(PanelConfigFormAction::SetAll(SourcesConfigDto::default()));
            }
        });
    }

    let render_input_card = |input_idx: usize, input: &ConfigInputDto| -> Html {
        let panel_enabled = input.panel_api.is_some();
        let errors = validate_panel(input.panel_api.as_ref());
        let has_errors = !errors.is_empty();
        let status_chip = if panel_enabled {
            if has_errors {
                html! { <Chip label={translate.t(LABEL_STATUS_INVALID)} class={Some("inactive".to_string())}/> }
            } else {
                html! { <Chip label={translate.t(LABEL_STATUS_READY)} class={Some("active".to_string())}/> }
            }
        } else {
            html! { <Chip label={translate.t(LABEL_STATUS_DISABLED)} class={Option::<String>::None}/> }
        };

        let on_toggle = {
            let form_state = form_state.clone();
            Callback::from(move |enabled: bool| {
                form_state.dispatch(PanelConfigFormAction::SetEnabled { input_idx, enabled });
            })
        };

        let on_url = {
            let form_state = form_state.clone();
            Callback::from(move |value: String| {
                form_state.dispatch(PanelConfigFormAction::SetPanelUrl { input_idx, url: value });
            })
        };

        let on_api_key = {
            let form_state = form_state.clone();
            Callback::from(move |value: String| {
                form_state.dispatch(PanelConfigFormAction::SetApiKey { input_idx, api_key: value });
            })
        };

        let panel = input.panel_api.as_ref();
        let url_val = panel.map(|p| p.url.clone()).unwrap_or_default();
        let api_key_val = panel.and_then(|p| p.api_key.clone()).unwrap_or_default();
        let client_info = panel.map(|p| p.query_parameter.client_info.as_slice()).unwrap_or(&[]);
        let client_new = panel.map(|p| p.query_parameter.client_new.as_slice()).unwrap_or(&[]);
        let client_renew = panel.map(|p| p.query_parameter.client_renew.as_slice()).unwrap_or(&[]);

        html! {
            <Card class="tp__config-view__card tp__panel-config-view__input-card">
                <div class="tp__panel-config-view__input-header">
                    <div class="tp__panel-config-view__input-title">
                        <h1>{ &input.name }</h1>
                        <div class="tp__panel-config-view__input-subtitle">{ format!("type={}", input.input_type) }</div>
                    </div>
                    <div class="tp__panel-config-view__input-status">
                        { status_chip }
                        { html_if!(*config_view_ctx.edit_mode, {
                            <div class="tp__panel-config-view__toggle">
                                <span class="lbl">{ translate.t(LABEL_ENABLED) }</span>
                                <ToggleSwitch value={panel_enabled} readonly={false} on_change={on_toggle} />
                            </div>
                        })}
                    </div>
                </div>

                {
                    if !panel_enabled {
                        html! { <div class="tp__panel-config-view__disabled-hint">{ translate.t(HINT_PANEL_ENABLE) }</div> }
                    } else if *config_view_ctx.edit_mode {
                        html! {
                            <>
                                <Input name="panel_url" label={Some(translate.t(LABEL_URL))} value={url_val} on_change={Some(on_url)} placeholder={Some("https://panel.example.tld/api.php".to_string())}/>
                                <Input name="panel_api_key" label={Some(translate.t(LABEL_API_KEY))} value={api_key_val} hidden={true} on_change={Some(on_api_key)} placeholder={Some("...".to_string())}/>
                                { render_param_editor(&form_state, true, input_idx, PanelSection::Info, translate.t(PanelSection::Info.label_key()), client_info) }
                                { render_param_editor(&form_state, true, input_idx, PanelSection::New, translate.t(PanelSection::New.label_key()), client_new) }
                                { render_param_editor(&form_state, true, input_idx, PanelSection::Renew, translate.t(PanelSection::Renew.label_key()), client_renew) }
                                { html_if!(has_errors, {
                                    <div class="tp__panel-config-view__errors">
                                        <h2>{ translate.t(LABEL_VALIDATION) }</h2>
                                        <ul>
                                            { for errors.iter().map(|e| html!{ <li>{ e }</li> }) }
                                        </ul>
                                    </div>
                                })}
                            </>
                        }
                    } else {
                        html!{
                            <>
                                <div class="tp__panel-config-view__summary">
                                    <div class="tp__form-field tp__form-field__text">
                                        <label>{ translate.t(LABEL_URL) }</label>
                                        <span class="tp__form-field__value">{ if url_val.is_empty() { "—".to_string() } else { url_val } }</span>
                                    </div>
                                    <div class="tp__form-field tp__form-field__text">
                                        <label>{ translate.t(LABEL_API_KEY) }</label>
                                        <span class="tp__form-field__value">{ if api_key_val.is_empty() { "—".to_string() } else { "••••••••".to_string() } }</span>
                                    </div>
                                    <div class="tp__form-field tp__form-field__text">
                                        <label>{ translate.t(PanelSection::Info.label_key()) }</label>
                                        <span class="tp__form-field__value">{ format!("{} params", client_info.len()) }</span>
                                    </div>
                                    <div class="tp__form-field tp__form-field__text">
                                        <label>{ translate.t(PanelSection::New.label_key()) }</label>
                                        <span class="tp__form-field__value">{ format!("{} params", client_new.len()) }</span>
                                    </div>
                                    <div class="tp__form-field tp__form-field__text">
                                        <label>{ translate.t(PanelSection::Renew.label_key()) }</label>
                                        <span class="tp__form-field__value">{ format!("{} params", client_renew.len()) }</span>
                                    </div>
                                </div>
                                { html_if!(has_errors, {
                                    <div class="tp__panel-config-view__errors">
                                        <h2>{ translate.t(LABEL_VALIDATION) }</h2>
                                        <ul>
                                            { for errors.iter().map(|e| html!{ <li>{ e }</li> }) }
                                        </ul>
                                    </div>
                                })}
                            </>
                        }
                    }
                }
            </Card>
        }
    };

    let inputs = form_state
        .form
        .inputs
        .iter()
        .enumerate()
        .collect::<Vec<_>>();

    html! {
        <div class="tp__panel-config-view tp__config-view-page">
            <div class="tp__config-view-page__title">{ translate.t(LABEL_PANEL_CONFIG) }</div>
            <div class="tp__panel-config-view__header tp__config-view-page__header">
                <Card class="tp__config-view__card tp__panel-config-view__info-card">
                    <h1>{ translate.t(LABEL_PANEL_CONFIG) }</h1>
                    <div class="tp__panel-config-view__info-text">
                        { translate.t(HINT_PANEL_INFO) }
                    </div>
                </Card>
            </div>
            <div class="tp__panel-config-view__body tp__config-view-page__body">
                { for inputs.into_iter().map(|(iidx, inp)| render_input_card(iidx, inp)) }
            </div>
        </div>
    }
}
