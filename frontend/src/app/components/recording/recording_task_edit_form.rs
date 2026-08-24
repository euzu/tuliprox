//! Inline form to edit a recording task's mutable fields.
//!
//! Renders directly under a row in `RecordingLibraryView`. The submit
//! callback calls `RecordingService::edit_task` and returns the user
//! to the listing.

use crate::{
    app::components::{datetime_input::DateTimeInput, number_input::NumberInput, text_button::TextButton},
    hooks::use_service_context,
    i18n::use_translation,
    services::{EditRecordingTaskRequest, RecordingService, RecordingTaskResponse},
};
use yew::prelude::*;

#[derive(Clone, PartialEq, Properties)]
#[allow(dead_code)]
pub struct TaskEditFormProps {
    pub task: RecordingTaskResponse,
    #[prop_or_default]
    pub on_done: Option<Callback<()>>,
}

#[function_component(TaskEditForm)]
pub fn task_edit_form(props: &TaskEditFormProps) -> Html {
    let services = use_service_context();
    let translate = use_translation();

    let task = props.task.clone();
    let program_start = use_state(|| task.recording.as_ref().and_then(|r| r.program_start).unwrap_or(0));
    let program_end = use_state(|| task.recording.as_ref().and_then(|r| r.program_end).unwrap_or(0));
    let pre_roll = use_state(|| task.recording.as_ref().map_or(0, |r| r.pre_roll_secs));
    let post_roll = use_state(|| task.recording.as_ref().map_or(0, |r| r.post_roll_secs));

    let id = task.id.clone();

    let on_program_start = {
        let program_start = program_start.clone();
        Callback::from(move |v: Option<i64>| {
            if let Some(v) = v {
                program_start.set(v);
            }
        })
    };
    let on_program_end = {
        let program_end = program_end.clone();
        Callback::from(move |v: Option<i64>| {
            if let Some(v) = v {
                program_end.set(v);
            }
        })
    };
    let on_pre = {
        let pre_roll = pre_roll.clone();
        Callback::from(move |v: Option<i64>| {
            if let Some(v) = v {
                pre_roll.set(v as u64);
            }
        })
    };
    let on_post = {
        let post_roll = post_roll.clone();
        Callback::from(move |v: Option<i64>| {
            if let Some(v) = v {
                post_roll.set(v as u64);
            }
        })
    };

    let on_submit_click = {
        let id = id.clone();
        let services = services.clone();
        let program_start = program_start.clone();
        let program_end = program_end.clone();
        let pre_roll = pre_roll.clone();
        let post_roll = post_roll.clone();
        let on_done = props.on_done.clone();
        let translate = translate.clone();
        Callback::from(move |_: String| {
            let id = id.clone();
            let svc = services.clone();
            let on_done = on_done.clone();
            let translate = translate.clone();
            let request = EditRecordingTaskRequest {
                program_start: Some(*program_start),
                program_end: Some(*program_end),
                pre_roll_secs: Some(*pre_roll),
                post_roll_secs: Some(*post_roll),
                program_title: None,
                channel_id: None,
                channel_name: None,
            };
            wasm_bindgen_futures::spawn_local(async move {
                match RecordingService::new().edit_task(&id, request).await {
                    Ok(()) => {
                        svc.toastr.success(translate.t("MESSAGES.RECORDING.TASK_UPDATED"));
                        if let Some(cb) = on_done {
                            cb.emit(());
                        }
                    }
                    Err(error) => {
                        log::warn!("recording edit failed: {error}");
                        svc.toastr.error(translate.t(error.i18n_key()));
                    }
                }
            });
        })
    };

    let on_cancel_click = {
        let on_done = props.on_done.clone();
        Callback::from(move |_: String| {
            if let Some(cb) = on_done.clone() {
                cb.emit(());
            }
        })
    };

    html! {
        <div class="tp__task-edit-form">
            <DateTimeInput
                name="program_start"
                label={translate.t("LABEL.RECORDING_FORM_START")}
                value={Some(*program_start)}
                on_change={Some(on_program_start)}
            />
            <DateTimeInput
                name="program_end"
                label={translate.t("LABEL.RECORDING_FORM_END")}
                value={Some(*program_end)}
                on_change={Some(on_program_end)}
            />
            <NumberInput
                name="pre_roll_secs"
                label={translate.t("LABEL.RECORDING_FORM_PRE_ROLL")}
                value={Some(*pre_roll as i64)}
                on_change={on_pre}
            />
            <NumberInput
                name="post_roll_secs"
                label={translate.t("LABEL.RECORDING_FORM_POST_ROLL")}
                value={Some(*post_roll as i64)}
                on_change={on_post}
            />
            <div class="tp__task-edit-form__actions">
                <TextButton
                    name="task_edit_submit"
                    icon=""
                    title={translate.t("LABEL.RECORDING_FORM_SAVE")}
                    onclick={on_submit_click}
                />
                <TextButton
                    name="task_edit_cancel"
                    icon=""
                    class="tp__button--secondary"
                    title={translate.t("LABEL.RECORDING_FORM_CANCEL")}
                    onclick={on_cancel_click}
                />
            </div>
        </div>
    }
}
