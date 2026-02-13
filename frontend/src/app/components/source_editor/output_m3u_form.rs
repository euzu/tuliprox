use crate::app::components::config::HasFormData;
use crate::app::components::{
    BlockId, BlockInstance, Card, EditMode, FilterInput, SourceEditorContext, TextButton,
};
use crate::{config_field_child, edit_field_bool, edit_field_text_option, generate_form_reducer};
use shared::model::{M3uTargetOutputDto, TargetOutputDto};
use std::rc::Rc;
use yew::{
    function_component, html, use_context, use_effect_with, use_reducer, Callback, Html,
    Properties, UseReducerHandle,
};
use yew_i18n::use_translation;

const LABEL_FILENAME: &str = "LABEL.FILENAME";
const LABEL_INCLUDE_TYPE_IN_URL: &str = "LABEL.INCLUDE_TYPE_IN_URL";
const LABEL_MASK_REDIRECT_URL: &str = "LABEL.MASK_REDIRECT_URL";
const LABEL_FILTER: &str = "LABEL.FILTER";

generate_form_reducer!(
    state: M3uTargetOutputFormState { form: M3uTargetOutputDto },
    action_name: M3uTargetOutputFormAction,
    fields {
        Filename => filename: Option<String>,
        IncludeTypeInUrl => include_type_in_url: bool,
        MaskRedirectUrl => mask_redirect_url: bool,
        Filter => filter: Option<String>,
    }
);

#[derive(Properties, PartialEq, Clone)]
pub struct M3uTargetOutputViewProps {
    pub(crate) block_id: BlockId,
    pub(crate) output: Option<Rc<M3uTargetOutputDto>>,
}

#[function_component]
pub fn M3uTargetOutputView(props: &M3uTargetOutputViewProps) -> Html {
    let translate = use_translation();
    let source_editor_ctx =
        use_context::<SourceEditorContext>().expect("SourceEditorContext not found");

    let output_form_state: UseReducerHandle<M3uTargetOutputFormState> =
        use_reducer(|| M3uTargetOutputFormState {
            form: M3uTargetOutputDto::default(),
            modified: false,
        });

    {
        let output_form_state = output_form_state.clone();
        let config_output = props.output.clone();

        use_effect_with(config_output, move |cfg| {
            if let Some(output) = cfg {
                output_form_state
                    .dispatch(M3uTargetOutputFormAction::SetAll(output.as_ref().clone()));
            } else {
                output_form_state.dispatch(M3uTargetOutputFormAction::SetAll(
                    M3uTargetOutputDto::default(),
                ));
            }
            || ()
        });
    }

    let render_output = || {
        let output_form_state_1 = output_form_state.clone();
        html! {
            <Card class="tp__config-view__card">
                { edit_field_text_option!(output_form_state, translate.t(LABEL_FILENAME), filename, M3uTargetOutputFormAction::Filename) }
                { edit_field_bool!(output_form_state, translate.t(LABEL_INCLUDE_TYPE_IN_URL), include_type_in_url, M3uTargetOutputFormAction::IncludeTypeInUrl) }
                { edit_field_bool!(output_form_state, translate.t(LABEL_MASK_REDIRECT_URL), mask_redirect_url, M3uTargetOutputFormAction::MaskRedirectUrl) }
                { config_field_child!(translate.t(LABEL_FILTER), {
                       html! {
                            <FilterInput filter={output_form_state_1.form.filter.clone()} on_change={Callback::from(move |new_filter| {
                                output_form_state_1.dispatch(M3uTargetOutputFormAction::Filter(new_filter));
                            })} />
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
                BlockInstance::Output(Rc::new(TargetOutputDto::M3u(output))),
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
                <TextButton class="secondary" name="cancel_m3u_output"
                    icon="Cancel"
                    title={ translate.t("LABEL.CANCEL")}
                    onclick={handle_cancel}></TextButton>
                <TextButton class="primary" name="apply_m3u_output"
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
