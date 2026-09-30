//! Standalone edit view for a single recording task.
//!
//! Looks up the task in the WS-driven library list and renders
//! `TaskEditForm`. Renders an i18n-keyed "task not found" message
//! when the id is unknown (e.g. it was deleted while the user was
//! editing it).

use crate::{
    app::components::recording::recording_task_edit_form::TaskEditForm, hooks::use_service_context,
    i18n::use_translation, model::EventMessage,
};
use shared::model::web_socket::ProtocolMessage;
use std::rc::Rc;
use yew::prelude::*;

/// Read the current task id from a context slot set by the router.
/// Kept as a context (not a prop) so the wrapper is decoupled from
/// whatever the home router does today.
#[derive(Clone, PartialEq, Default)]
#[allow(dead_code)]
pub struct EditingTaskId(pub Rc<Option<String>>);

#[function_component(RecordingEditView)]
pub fn recording_edit_view() -> Html {
    let services = use_service_context();
    let translate = use_translation();
    let editing = use_context::<EditingTaskId>().unwrap_or_default();
    let tasks = use_state(|| Rc::new(Vec::<shared::model::RecordingTaskDto>::new()));

    // Subscribe to the same WS stream the library view uses. The
    // backend broadcasts the same per-session filtered snapshot to
    // every subscriber, so we get the same live updates without a
    // second fetch.
    {
        let tasks = tasks.clone();
        let svc = services.clone();
        use_effect_with((), move |()| {
            let sid = svc.event.subscribe(move |msg| {
                if let EventMessage::RecordingSnapshot { tasks: incoming, .. } = msg {
                    let mapped = (*incoming).clone();
                    tasks.set(Rc::new(mapped));
                }
            });
            let _ = svc.websocket.send_message(ProtocolMessage::RecordingSnapshotRequest);
            move || svc.event.unsubscribe(sid)
        });
    }

    let on_done = Callback::from(move |(): ()| { /* router picks this up */ });

    let body = match (*editing.0).as_ref() {
        Some(id) => (*tasks).iter().find(|t| &t.id == id).cloned().map_or_else(
            || html! { <p>{ translate.t("MESSAGES.RECORDING.TASK_NOT_FOUND") }</p> },
            |task| html! { <TaskEditForm task={task} on_done={on_done} /> },
        ),
        None => html! { <p>{ translate.t("MESSAGES.RECORDING.NO_TASK_SELECTED") }</p> },
    };

    html! {
        <div class="tp__recording-edit-view tp__list-view">
            <div class="tp__recording-edit-view__body tp__list-view__body">
                <h1>{ translate.t("LABEL.RECORDING_EDIT_TITLE") }</h1>
                { body }
            </div>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editing_task_id_default_is_none() {
        let ctx = EditingTaskId::default();
        assert!(ctx.0.is_none());
    }
}
