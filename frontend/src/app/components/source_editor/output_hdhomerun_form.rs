use crate::app::components::config::HasFormData;
use crate::app::components::select::Select;
use crate::app::components::{
    BlockId, BlockInstance, Card, DropDownOption, DropDownSelection, EditMode, SourceEditorContext,
    TextButton,
};
use crate::{config_field_child, edit_field_text, generate_form_reducer};
use shared::model::{HdHomeRunTargetOutputDto, TargetOutputDto, TargetType};
use std::rc::Rc;
use yew::{
    function_component, html, use_context, use_effect_with, use_memo, use_reducer, Callback, Html,
    Properties, UseReducerHandle,
};
use yew_i18n::use_translation;

const LABEL_DEVICE: &str = "LABEL.DEVICE";
const LABEL_USERNAME: &str = "LABEL.USERNAME";
const LABEL_USE_OUTPUT: &str = "LABEL.USE_OUTPUT";

generate_form_reducer!(
    state: HdHomeRunTargetOutputFormState { form: HdHomeRunTargetOutputDto },
    action_name: HdHomeRunTargetOutputFormAction,
    fields {
        Device => device: String,
        Username => username: String,
        UseOutput => use_output: Option<TargetType>,
    }
);

#[derive(Properties, PartialEq, Clone)]
pub struct HdHomeRunTargetOutputViewProps {
    pub(crate) block_id: BlockId,
    pub(crate) output: Option<Rc<HdHomeRunTargetOutputDto>>,
}

#[function_component]
pub fn HdHomeRunTargetOutputView(props: &HdHomeRunTargetOutputViewProps) -> Html {
    let translate = use_translation();
    let source_editor_ctx =
        use_context::<SourceEditorContext>().expect("SourceEditorContext not found");

    let output_form_state: UseReducerHandle<HdHomeRunTargetOutputFormState> =
        use_reducer(|| HdHomeRunTargetOutputFormState {
            form: HdHomeRunTargetOutputDto::default(),
            modified: false,
        });

    let target_types = use_memo(output_form_state.form.use_output, |use_output| {
        let default_type = use_output.unwrap_or(TargetType::M3u);
        [TargetType::M3u, TargetType::Xtream]
            .iter()
            .map(|t| DropDownOption {
                id: t.to_string(),
                label: html! { t.to_string() },
                selected: *t == default_type,
            })
            .collect::<Vec<DropDownOption>>()
    });

    {
        let output_form_state = output_form_state.clone();
        let config_output = props.output.clone();

        use_effect_with(config_output, move |cfg| {
            if let Some(output) = cfg {
                output_form_state.dispatch(HdHomeRunTargetOutputFormAction::SetAll(
                    output.as_ref().clone(),
                ));
            } else {
                output_form_state.dispatch(HdHomeRunTargetOutputFormAction::SetAll(
                    HdHomeRunTargetOutputDto::default(),
                ));
            }
            || ()
        });
    }

    let render_output = || {
        let output_form_state_1 = output_form_state.clone();
        html! {
            <Card class="tp__config-view__card">
                { edit_field_text!(output_form_state, translate.t(LABEL_DEVICE), device, HdHomeRunTargetOutputFormAction::Device) }
                { edit_field_text!(output_form_state, translate.t(LABEL_USERNAME), username, HdHomeRunTargetOutputFormAction::Username) }
                { config_field_child!(translate.t(LABEL_USE_OUTPUT), {
                    html! {
                        <Select
                            name={"use_output"}
                            multi_select={false}
                            on_select={Callback::from(move |(_, selections):(String, DropDownSelection)| {
                                match selections {
                                    DropDownSelection::Empty => {
                                        output_form_state_1.dispatch(HdHomeRunTargetOutputFormAction::UseOutput(Some(TargetType::M3u)));
                                    }
                                    DropDownSelection::Single(option) => {
                                        output_form_state_1.dispatch(HdHomeRunTargetOutputFormAction::UseOutput(Some(option.parse::<TargetType>().unwrap_or(TargetType::M3u))));
                                    }
                                    DropDownSelection::Multi(options) => {
                                        if let Some(first) = options.first() {
                                            output_form_state_1.dispatch(HdHomeRunTargetOutputFormAction::UseOutput(Some(first.parse::<TargetType>().unwrap_or(TargetType::M3u))));
                                        }
                                    }
                                }
                            })}
                            options={target_types.clone()}
                        />
                    }
                })}
            </Card>
        }
    };

    let handle_apply = {
        let source_editor_ctx = source_editor_ctx.clone();
        let output_form_state = output_form_state.clone();
        let block_id = props.block_id;
        Callback::from(move |_| {
            let output = output_form_state.data().clone();
            source_editor_ctx.on_form_change.emit((
                block_id,
                BlockInstance::Output(Rc::new(TargetOutputDto::HdHomeRun(output))),
            ));
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
                <TextButton class="secondary" name="cancel_hdhomerun_output"
                    icon="Cancel"
                    title={ translate.t("LABEL.CANCEL")}
                    onclick={handle_cancel}></TextButton>
                <TextButton class="primary" name="apply_hdhomerun_output"
                    icon="Accept"
                    title={ translate.t("LABEL.OK")}
                    onclick={handle_apply}></TextButton>
            </div>
            <div class="tp__input-form__body">
                { render_output() }
            </div>
        </div>
    }
}
