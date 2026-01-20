use crate::app::components::config::HasFormData;
use crate::app::components::select::Select;
use crate::app::components::{BlockId, BlockInstance, Card, DropDownOption, DropDownSelection, EditMode, FilterInput, IconButton, Panel, SourceEditorContext, TextButton};
use crate::{config_field_child, edit_field_bool, edit_field_list_option, edit_field_text, edit_field_text_option, generate_form_reducer};
use shared::model::{StrmExportStyle, StrmTargetOutputDto, TargetOutputDto};
use std::fmt::Display;
use std::rc::Rc;
use std::str::FromStr;
use yew::{function_component, html, use_context, use_effect_with, use_memo, use_reducer, use_state, Callback, Html, Properties, UseReducerHandle};
use yew_i18n::use_translation;
use shared::error::TuliproxError;
use shared::info_err_res;

const LABEL_DIRECTORY: &str = "LABEL.DIRECTORY";
const LABEL_USERNAME: &str = "LABEL.USERNAME";
const LABEL_EXPORT_STYLE: &str = "LABEL.EXPORT_STYLE";
const LABEL_FLAT: &str = "LABEL.FLAT";
const LABEL_UNDERSCORE_WHITESPACE: &str = "LABEL.UNDERSCORE_WHITESPACE";
const LABEL_CLEANUP: &str = "LABEL.CLEANUP";
const LABEL_STRM_PROPS: &str = "LABEL.STRM_PROPS";
const LABEL_FILTER: &str = "LABEL.FILTER";
const LABEL_ADD_QUALITY_TO_FILENAME: &str = "LABEL.ADD_QUALITY_TO_FILENAME";
const LABEL_ADD_PROPERTY: &str = "LABEL.ADD_PROPERTY";
const LABEL_MAIN: &str = "LABEL.MAIN_CONFIG";
const LABEL_OPTIONS: &str = "LABEL.OPTIONS";

#[derive(Copy, Clone, PartialEq, Eq)]
enum StrmFormPage {
    Main,
    Options,
}


impl StrmFormPage {
    const MAIN: &str = "Main";
    const OPTIONS: &str = "Options";
}

impl FromStr for StrmFormPage {
    type Err = TuliproxError;

    fn from_str(s: &str) -> Result<Self, TuliproxError> {
        match s {
            Self::MAIN => Ok(StrmFormPage::Main),
            Self::OPTIONS => Ok(StrmFormPage::Options),
            _ => info_err_res!("Unknown strm output form page: {s}"),
        }
    }
}


impl Display for StrmFormPage {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{}", match *self {
            StrmFormPage::Main => Self::MAIN,
            StrmFormPage::Options => Self::OPTIONS,
        })
    }
}

generate_form_reducer!(
    state: StrmTargetOutputFormState { form: StrmTargetOutputDto },
    action_name: StrmTargetOutputFormAction,
    fields {
        Directory => directory: String,
        Username => username: Option<String>,
        Style => style: StrmExportStyle,
        Flat => flat: bool,
        UnderscoreWhitespace => underscore_whitespace: bool,
        Cleanup => cleanup: bool,
        StrmProps => strm_props: Option<Vec<String>>,
        Filter => filter: Option<String>,
        AddQualityToFilename => add_quality_to_filename: bool,
    }
);

#[derive(Properties, PartialEq, Clone)]
pub struct StrmTargetOutputViewProps {
    pub(crate) block_id: BlockId,
    pub(crate) output: Option<Rc<StrmTargetOutputDto>>,
}

#[function_component]
pub fn StrmTargetOutputView(props: &StrmTargetOutputViewProps) -> Html {
    let translate = use_translation();
    let source_editor_ctx = use_context::<SourceEditorContext>().expect("SourceEditorContext not found");

    let output_form_state: UseReducerHandle<StrmTargetOutputFormState> =
        use_reducer(|| StrmTargetOutputFormState {
            form: StrmTargetOutputDto::default(),
            modified: false,
        });

    let view_visible = use_state(|| StrmFormPage::Main);

    let handle_menu_click = {
        let active_menu = view_visible.clone();
        Callback::from(move |(name, _): (String, _)| {
            if let Ok(view_type) = StrmFormPage::from_str(&name) {
                active_menu.set(view_type);
            }
        })
    };

    let export_styles = use_memo(output_form_state.form.style, |style| {
        let default_style = *style;
        [
            StrmExportStyle::Kodi,
            StrmExportStyle::Plex,
            StrmExportStyle::Emby,
            StrmExportStyle::Jellyfin,
        ]
            .iter()
            .map(|s| DropDownOption {
                id: s.to_string(),
                label: html! { s.to_string() },
                selected: *s == default_style,
            }).collect::<Vec<DropDownOption>>()
    });

    {
        let output_form_state = output_form_state.clone();
        let config_output = props.output.clone();

        use_effect_with(config_output, move |cfg| {
            if let Some(output) = cfg {
                output_form_state.dispatch(StrmTargetOutputFormAction::SetAll(output.as_ref().clone()));
            } else {
                output_form_state.dispatch(StrmTargetOutputFormAction::SetAll(StrmTargetOutputDto::default()));
            }
            || ()
        });
    }

    let render_main = || {
        let output_form_state_1 = output_form_state.clone();
        let output_form_state_2 = output_form_state.clone();
        html! {
            <Card class="tp__config-view__card">
                { edit_field_text!(output_form_state, translate.t(LABEL_DIRECTORY), directory, StrmTargetOutputFormAction::Directory) }
                { edit_field_text_option!(output_form_state, translate.t(LABEL_USERNAME), username, StrmTargetOutputFormAction::Username) }
                { config_field_child!(translate.t(LABEL_EXPORT_STYLE), {
                    html! {
                        <Select
                            name={"export_style"}
                            multi_select={false}
                            on_select={Callback::from(move |(_, selections):(String, DropDownSelection)| {
                                match selections {
                                    DropDownSelection::Empty => {
                                        output_form_state_1.dispatch(StrmTargetOutputFormAction::Style(StrmExportStyle::Kodi));
                                    }
                                    DropDownSelection::Single(option) => {
                                        output_form_state_1.dispatch(StrmTargetOutputFormAction::Style(option.parse::<StrmExportStyle>().unwrap_or(StrmExportStyle::Kodi)));
                                    }
                                    DropDownSelection::Multi(options) => {
                                        if let Some(first) = options.first() {
                                            output_form_state_1.dispatch(StrmTargetOutputFormAction::Style(first.parse::<StrmExportStyle>().unwrap_or(StrmExportStyle::Kodi)));
                                        }
                                    }
                                }
                            })}
                            options={export_styles.clone()}
                        />
                    }
                })}
                { config_field_child!(translate.t(LABEL_FILTER), {
                   html! {
                        <FilterInput filter={output_form_state_2.form.filter.clone()} on_change={Callback::from(move |new_filter| {
                            output_form_state_2.dispatch(StrmTargetOutputFormAction::Filter(new_filter));
                        })} />
                   }
                })}
            </Card>
        }
    };

    let render_options = || {
        html! {
            <Card class="tp__config-view__card">
                { edit_field_bool!(output_form_state, translate.t(LABEL_FLAT), flat, StrmTargetOutputFormAction::Flat) }
                { edit_field_bool!(output_form_state, translate.t(LABEL_UNDERSCORE_WHITESPACE), underscore_whitespace, StrmTargetOutputFormAction::UnderscoreWhitespace) }
                { edit_field_bool!(output_form_state, translate.t(LABEL_CLEANUP), cleanup, StrmTargetOutputFormAction::Cleanup) }
                { edit_field_bool!(output_form_state, translate.t(LABEL_ADD_QUALITY_TO_FILENAME), add_quality_to_filename, StrmTargetOutputFormAction::AddQualityToFilename) }
                { edit_field_list_option!(output_form_state, translate.t(LABEL_STRM_PROPS), strm_props, StrmTargetOutputFormAction::StrmProps, translate.t(LABEL_ADD_PROPERTY)) }
            </Card>
        }
    };

    let render_edit_mode = || {
        html! {
            <div class="tp__source-editor-form__body">
                <div class="tp__source-editor-form__body__pages">
                    <Panel value={StrmFormPage::Main.to_string()} active={view_visible.to_string()}>
                        {render_main()}
                    </Panel>
                    <Panel value={StrmFormPage::Options.to_string()} active={view_visible.to_string()}>
                        {render_options()}
                    </Panel>
                </div>
            </div>
        }
    };

    let render_sidebar = || {
        html! {
            <div class="tp__source-editor-form__sidebar">
            <IconButton class={format!("tp__app-sidebar-menu--{}{}", StrmFormPage::Main, if *view_visible == StrmFormPage::Main { " active" } else {""})}  icon="Settings" hint={translate.t(LABEL_MAIN)} name={StrmFormPage::Main.to_string()} onclick={&handle_menu_click}></IconButton>
            <IconButton class={format!("tp__app-sidebar-menu--{}{}", StrmFormPage::Options, if *view_visible == StrmFormPage::Options { " active" } else {""})}  icon="Options" hint={translate.t(LABEL_OPTIONS)} name={StrmFormPage::Options.to_string()} onclick={&handle_menu_click}></IconButton>
          </div>
        }
    };

    let handle_apply = {
        let source_editor_ctx = source_editor_ctx.clone();
        let output_form_state = output_form_state.clone();
        let block_id = props.block_id;
        Callback::from(move |_| {
            let output = output_form_state.data().clone();
            source_editor_ctx.on_form_change.emit((block_id, BlockInstance::Output(Rc::new(TargetOutputDto::Strm(output)))));
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
                <TextButton class="secondary" name="cancel_strm_output"
                    icon="Cancel"
                    title={ translate.t("LABEL.CANCEL")}
                    onclick={handle_cancel}></TextButton>
                <TextButton class="primary" name="apply_strm_output"
                    icon="Accept"
                    title={ translate.t("LABEL.OK")}
                    onclick={handle_apply}></TextButton>
            </div>
            <div class="tp__source-editor-form__content">
                { render_sidebar() }
                { render_edit_mode() }
            </div>
        </div>
    }
}
