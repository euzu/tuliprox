use super::{
    maintenance_dialog::MaintenanceDialog,
    maintenance_state::{is_alert, merge_loaded, preview_names},
};
use crate::{
    app::{
        components::{ActionCard, TextButton},
        CardContext,
    },
    hooks::use_service_context,
    i18n::use_translation,
    model::EventMessage,
};
use shared::model::{InputMaintenanceStateDto, Permission};
use std::rc::Rc;
use yew::prelude::*;

const ALERT_CLASS: &str = "tp__card--maintenance";

fn dialog_portal(content: Html) -> Html {
    match web_sys::window().and_then(|window| window.document()).and_then(|document| document.body()) {
        Some(body) => yew::create_portal(content, body.into()),
        None => content,
    }
}

#[component]
pub fn MaintenanceActionCard() -> Html {
    let services = use_service_context();
    let translate = use_translation();
    let card_ctx = use_context::<CardContext>();
    let state = use_state(|| Rc::new(InputMaintenanceStateDto::default()));
    // The latest state, readable from the subscription and from a load in flight without going
    // through a stale state handle.
    let latest = use_mut_ref(|| Rc::new(InputMaintenanceStateDto::default()));
    // Counts change events; a load that saw a newer event than the one it started after is stale.
    let event_generation = use_mut_ref(|| 0_u64);
    let dialog_open = use_state(|| false);
    let can_read = services.auth.has_permission(Permission::MaintenanceRead);
    let can_write = services.auth.has_permission(Permission::MaintenanceWrite);

    let apply_state = {
        let state = state.clone();
        let latest = latest.clone();
        Callback::from(move |next: InputMaintenanceStateDto| {
            let next = Rc::new(next);
            *latest.borrow_mut() = Rc::clone(&next);
            state.set(next);
        })
    };

    let reload = {
        let services = services.clone();
        let translate = translate.clone();
        let apply_state = apply_state.clone();
        let latest = latest.clone();
        let event_generation = event_generation.clone();
        Callback::from(move |()| {
            let services = services.clone();
            let translate = translate.clone();
            let apply_state = apply_state.clone();
            let latest = latest.clone();
            let event_generation = event_generation.clone();
            let started_at = *event_generation.borrow();
            wasm_bindgen_futures::spawn_local(async move {
                match services.maintenance.get().await {
                    Ok(loaded) => {
                        let stale = *event_generation.borrow() != started_at;
                        let current = Rc::clone(&latest.borrow());
                        apply_state.emit(merge_loaded(&current, loaded, stale));
                    }
                    Err(_) => services.toastr.error(translate.t("MESSAGES.MAINTENANCE.LOAD_FAILED")),
                }
            });
        })
    };

    {
        let services = services.clone();
        let apply_state = apply_state.clone();
        let latest = latest.clone();
        let event_generation = event_generation.clone();
        let reload = reload.clone();
        use_effect_with(can_read, move |&can_read| {
            let subscription = can_read.then(|| {
                reload.emit(());
                services.event.subscribe(move |message| match message {
                    EventMessage::InputMaintenanceChanged(entries) => {
                        *event_generation.borrow_mut() += 1;
                        let inputs = latest.borrow().inputs.clone();
                        apply_state.emit(InputMaintenanceStateDto { entries: (*entries).clone(), inputs });
                    }
                    // Events sent while the socket was down are lost, and a source reload can
                    // add inputs: read the state again.
                    EventMessage::WebSocketStatus(true) | EventMessage::ConfigChange(_) => reload.emit(()),
                    _ => {}
                })
            });
            move || {
                if let Some(subscription) = subscription {
                    services.event.unsubscribe(subscription);
                }
            }
        });
    }

    {
        let alert = is_alert(&state);
        use_effect_with(alert, move |&alert| {
            if let Some(card_ctx) = card_ctx {
                card_ctx.custom_class.set(if alert { ALERT_CLASS.to_string() } else { String::new() });
            }
            || ()
        });
    }

    if !can_read {
        return Html::default();
    }

    let subtitle = if is_alert(&state) {
        format!(
            "{}: {}",
            if state.entries.len() == 1 {
                translate.t("LABEL.MAINTENANCE_COUNT_ONE")
            } else {
                translate.t("LABEL.MAINTENANCE_COUNT").replace("{count}", &state.entries.len().to_string())
            },
            preview_names(&state, 3)
        )
    } else {
        translate.t("LABEL.MAINTENANCE_NONE")
    };

    let open_dialog = {
        let dialog_open = dialog_open.clone();
        Callback::from(move |_: String| dialog_open.set(true))
    };
    let close_dialog = {
        let dialog_open = dialog_open.clone();
        Callback::from(move |()| dialog_open.set(false))
    };

    html! {
        <>
          <ActionCard icon="Warning" classname="tp__maintenance-card" title={translate.t("LABEL.MAINTENANCE")} subtitle={subtitle}>
            <TextButton name="maintenance" title={translate.t("LABEL.MANAGE")} icon="Settings" onclick={open_dialog} />
          </ActionCard>
          if *dialog_open {
            // Rendered into <body>: inside the card the backdrop would share the card's stacking
            // context and let pointer events reach the dashboard behind it.
            { dialog_portal(html! {
                <MaintenanceDialog state={(*state).clone()} can_write={can_write} on_state={apply_state} on_close={close_dialog} />
            }) }
          }
        </>
    }
}
