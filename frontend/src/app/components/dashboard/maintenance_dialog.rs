use super::maintenance_state::{
    add_all_to_draft, add_to_draft, pending_changes, reason_too_long, relative_duration, PendingChanges,
};
use crate::{
    app::components::{
        input::Input, selection_first_owned, CustomDialog, DropDownOption, DropDownSelection, IconButton, Select,
        TextButton,
    },
    config_field_child,
    hooks::use_service_context,
    i18n::use_translation,
    services::MaintenanceService,
};
use shared::model::{MaintenanceEntries, MaintenanceStateDto};
use std::rc::Rc;
use wasm_bindgen::JsValue;
use yew::prelude::*;

#[derive(Properties, Clone, PartialEq)]
pub struct MaintenanceDialogProps {
    pub state: Rc<MaintenanceStateDto>,
    pub can_write: bool,
    pub on_state: Callback<MaintenanceStateDto>,
    pub on_close: Callback<()>,
}

#[allow(clippy::cast_precision_loss)]
fn local_time(since: i64) -> String {
    let millis = since.saturating_mul(1000) as f64;
    String::from(js_sys::Date::new(&JsValue::from_f64(millis)).to_locale_string("default", &JsValue::UNDEFINED))
}

#[allow(clippy::cast_possible_truncation)]
fn now_secs() -> i64 { (js_sys::Date::now() / 1000.0) as i64 }

/// Sends the pending changes one by one and reads the resulting state back.
async fn apply_changes(
    service: &MaintenanceService,
    changes: PendingChanges,
) -> Result<MaintenanceStateDto, crate::error::Error> {
    for name in &changes.deletes {
        service.remove(name).await?;
    }
    for (name, reason) in changes.puts {
        service.add(&name, reason).await?;
    }
    service.get().await
}

/// Edits a draft of the inputs in maintenance. Adding, removing and the bulk actions only change
/// the draft; Save sends the difference to the server, Close discards it.
#[component]
pub fn MaintenanceDialog(props: &MaintenanceDialogProps) -> Html {
    let services = use_service_context();
    let translate = use_translation();
    let draft = {
        let entries = props.state.entries.clone();
        use_state(move || entries)
    };
    let selected = use_state(|| None::<String>);
    let reason = use_state(String::new);
    let pending = use_state(|| false);

    let too_long = reason_too_long(&reason);
    let changes = pending_changes(&props.state.entries, &draft);

    let on_add = {
        let draft = draft.clone();
        let selected = selected.clone();
        let reason = reason.clone();
        Callback::from(move |(_, event): (String, MouseEvent)| {
            event.prevent_default();
            let Some(name) = (*selected).clone() else { return };
            if reason_too_long(&reason) {
                return;
            }
            draft.set(add_to_draft(&draft, &name, &reason, now_secs()));
            selected.set(None);
            reason.set(String::new());
        })
    };

    let on_add_all = {
        let draft = draft.clone();
        let reason = reason.clone();
        let inputs = props.state.inputs.clone();
        Callback::from(move |_: String| {
            if !reason_too_long(&reason) {
                draft.set(add_all_to_draft(&draft, &inputs, &reason, now_secs()));
            }
        })
    };

    let on_remove_all = {
        let draft = draft.clone();
        Callback::from(move |_: String| draft.set(MaintenanceEntries::new()))
    };

    let on_save = {
        let services = services.clone();
        let translate = translate.clone();
        let on_state = props.on_state.clone();
        let on_close = props.on_close.clone();
        let pending = pending.clone();
        let saved = props.state.entries.clone();
        let draft = draft.clone();
        Callback::from(move |_: String| {
            let changes = pending_changes(&saved, &draft);
            if changes.is_empty() {
                on_close.emit(());
                return;
            }
            let services = services.clone();
            let translate = translate.clone();
            let on_state = on_state.clone();
            let on_close = on_close.clone();
            let pending = pending.clone();
            pending.set(true);
            wasm_bindgen_futures::spawn_local(async move {
                match apply_changes(&services.maintenance, changes).await {
                    Ok(next) => {
                        on_state.emit(next);
                        on_close.emit(());
                    }
                    // A partial save is visible through the change event; the draft stays for a retry.
                    Err(_) => services.toastr.error(translate.t("MESSAGES.MAINTENANCE.SAVE_FAILED")),
                }
                pending.set(false);
            });
        })
    };

    let now = now_secs();
    let rows = draft.iter().map(|(name, entry)| {
        let unsaved = props.state.entries.get(name) != Some(entry);
        let on_remove = {
            let draft = draft.clone();
            let name = name.clone();
            Callback::from(move |_: String| {
                let mut next = (*draft).clone();
                next.remove(&name);
                draft.set(next);
            })
        };
        let on_edit = {
            let selected = selected.clone();
            let reason = reason.clone();
            let name = name.clone();
            let remark = entry.reason.clone().unwrap_or_default();
            Callback::from(move |_: String| {
                selected.set(Some(name.clone()));
                reason.set(remark.clone());
            })
        };
        html! {
            <tr key={name.clone()} class={classes!(unsaved.then_some("tp__maintenance-dialog__row--unsaved"))}>
                <td>{name}</td>
                <td title={local_time(entry.since)}>{relative_duration(entry.since, now)}</td>
                <td>{entry.reason.clone().unwrap_or_default()}</td>
                if props.can_write {
                    <td class="tp__maintenance-dialog__actions">
                        <TextButton name="edit" icon="Edit" title={translate.t("LABEL.EDIT")} onclick={on_edit}/>
                        <TextButton name="remove" icon="Delete" class="secondary" title={translate.t("LABEL.DELETE")} onclick={on_remove}/>
                    </td>
                }
            </tr>
        }
    });

    // Inputs not in the draft yet, plus the one being edited.
    let options = {
        let mut names: Vec<&str> =
            props.state.inputs.iter().map(String::as_str).filter(|name| !draft.contains_key(*name)).collect();
        if let Some(editing) = selected.as_deref() {
            if !names.contains(&editing) {
                names.insert(0, editing);
            }
        }
        Rc::new(
            names
                .into_iter()
                .map(|name| DropDownOption::new(name, html! { name }, selected.as_deref() == Some(name)))
                .collect::<Vec<_>>(),
        )
    };
    let on_select = {
        let selected = selected.clone();
        Callback::from(move |(_, selection): (String, DropDownSelection)| {
            selected.set(selection_first_owned(selection));
        })
    };
    let on_reason = {
        let reason = reason.clone();
        Callback::from(move |value: String| reason.set(value))
    };
    let close = props.on_close.clone();
    let close_button = {
        let close = props.on_close.clone();
        Callback::from(move |_: String| close.emit(()))
    };
    let busy = *pending;

    html! {
        <CustomDialog class={Some("tp__maintenance-dialog".to_owned())} open={true} modal={true} close_on_backdrop_click={true}
            on_close={Some(close)} aria_label={Some(translate.t("LABEL.MAINTENANCE"))}>
            <h2>{translate.t("LABEL.MAINTENANCE")}</h2>
            if props.can_write {
                <div class="tp__maintenance-dialog__form">
                    { config_field_child!(translate.t("LABEL.INPUT"), "MAINTENANCE.INPUT", {
                        html! {
                            <Select name="maintenance_input" multi_select={false} required={true}
                                on_select={on_select} options={options}/>
                        }
                    })}
                    <Input name="maintenance_reason" field_id={Some("MAINTENANCE.REASON".to_string())}
                        label={Some(translate.t("LABEL.MAINTENANCE_REASON"))}
                        value={(*reason).clone()} on_change={on_reason}
                        error={too_long.then(|| translate.t("MESSAGES.MAINTENANCE.REASON_TOO_LONG"))}/>
                    <IconButton name="add_maintenance" icon="Add" hint={translate.t("LABEL.MAINTENANCE_ADD")}
                        class={classes!("tp__maintenance-dialog__add", "primary", (selected.is_none() || too_long).then_some("disabled")).to_string()}
                        onclick={on_add}/>
                </div>
            }
            <div class="tp__maintenance-dialog__list">
                if draft.is_empty() {
                    <p class="tp__maintenance-dialog__empty">{translate.t("LABEL.MAINTENANCE_NONE")}</p>
                } else {
                    <table class="tp__maintenance-dialog__table">
                        <thead><tr>
                            <th>{translate.t("LABEL.INPUT")}</th>
                            <th>{translate.t("LABEL.MAINTENANCE_SINCE")}</th>
                            <th>{translate.t("LABEL.MAINTENANCE_REASON")}</th>
                            if props.can_write { <th></th> }
                        </tr></thead>
                        <tbody>{for rows}</tbody>
                    </table>
                }
            </div>
            <div class="tp__dialog__toolbar">
                <div class="tp__dialog__toolbar-left">
                    if props.can_write {
                        <TextButton name="add_all" icon="Add" title={translate.t("LABEL.MAINTENANCE_ADD_ALL")}
                            disabled={busy || too_long} onclick={on_add_all}/>
                        <TextButton name="remove_all" icon="Clear" class="secondary" title={translate.t("LABEL.MAINTENANCE_REMOVE_ALL")}
                            disabled={busy || draft.is_empty()} onclick={on_remove_all}/>
                    }
                </div>
                <div class="tp__dialog__toolbar-right">
                    <TextButton name="close" class="secondary" icon="Cancel" title={translate.t("LABEL.CLOSE")} onclick={close_button}/>
                    if props.can_write {
                        <TextButton name="save" class="primary" icon="Save" title={translate.t("LABEL.SAVE")}
                            disabled={busy || changes.is_empty()} onclick={on_save}/>
                    }
                </div>
            </div>
        </CustomDialog>
    }
}
