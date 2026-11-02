//! Marketplace source management: the list of configured sources with their sync state, the add/edit dialog, remove and sync.
use crate::{
    app::components::{input::Input, ConfirmDialog, ContentDialog, TextButton, ToggleSwitch},
    hooks::use_service_context,
    i18n::use_translation,
    model::{DialogAction, DialogActions, DialogResult},
    services::PluginActionError,
};
use js_sys::Date;
use shared::model::{PluginRepositoryDto, PluginRepositoryInfoDto};
use std::rc::Rc;
use wasm_bindgen::JsValue;
use yew::{component, html, platform::spawn_local, use_state, Callback, Html, Properties};

/// Local time as `YYYY-MM-DD HH:MM:SS` of a unix-seconds timestamp.
fn format_sync_time(seconds: i64) -> String {
    #[allow(clippy::cast_precision_loss)]
    let date = Date::new(&JsValue::from_f64(seconds as f64 * 1_000.0));
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        date.get_full_year(),
        date.get_month() + 1,
        date.get_date(),
        date.get_hours(),
        date.get_minutes(),
        date.get_seconds()
    )
}

#[derive(Properties, Clone, PartialEq)]
struct SourceDialogProps {
    /// The source being edited; `None` adds a new one.
    initial: Option<PluginRepositoryInfoDto>,
    on_saved: Callback<()>,
    on_close: Callback<()>,
}

/// The add/edit dialog of one source. The server's `422` and `409` problems stay in the dialog.
#[component]
fn SourceDialog(props: &SourceDialogProps) -> Html {
    let translate = use_translation();
    let services = use_service_context();
    let editing = props.initial.is_some();
    let id = use_state(|| props.initial.as_ref().map(|source| source.id.clone()).unwrap_or_default());
    let name = use_state(|| props.initial.as_ref().map(|source| source.name.clone()).unwrap_or_default());
    let url = use_state(|| props.initial.as_ref().map(|source| source.url.clone()).unwrap_or_default());
    let enabled = use_state(|| props.initial.as_ref().is_none_or(|source| source.enabled));
    let busy = use_state(|| false);
    let error = use_state(|| None::<PluginActionError>);

    let save = {
        let services = services.clone();
        let id = id.clone();
        let name = name.clone();
        let url = url.clone();
        let enabled = enabled.clone();
        let busy = busy.clone();
        let error = error.clone();
        let original = props.initial.as_ref().map(|source| source.id.clone());
        let on_saved = props.on_saved.clone();
        let on_close = props.on_close.clone();
        Callback::from(move |()| {
            if *busy {
                return;
            }
            busy.set(true);
            let body = PluginRepositoryDto {
                id: id.trim().to_owned(),
                name: name.trim().to_owned(),
                url: url.trim().to_owned(),
                enabled: *enabled,
            };
            let services = services.clone();
            let original = original.clone();
            let busy = busy.clone();
            let error = error.clone();
            let on_saved = on_saved.clone();
            let on_close = on_close.clone();
            spawn_local(async move {
                let result = match original.as_deref() {
                    Some(original) => services.plugins.update_repository(original, &body).await,
                    None => services.plugins.add_repository(&body).await,
                };
                match result {
                    Ok(_) => {
                        on_saved.emit(());
                        on_close.emit(());
                    }
                    Err(err) => error.set(Some(err)),
                }
                busy.set(false);
            });
        })
    };
    let on_result = {
        let on_close = props.on_close.clone();
        Callback::from(move |result: DialogResult| match result {
            DialogResult::Cancel => on_close.emit(()),
            DialogResult::Ok => save.emit(()),
        })
    };
    let actions = DialogActions {
        left: Some(vec![DialogAction::new(
            "cancel",
            "LABEL.CANCEL",
            DialogResult::Cancel,
            Some("Close".to_owned()),
            Some("secondary".to_owned()),
        )]),
        right: vec![DialogAction::new(
            "save",
            "LABEL.SAVE",
            DialogResult::Ok,
            Some("Save".to_owned()),
            Some("primary".to_owned()),
        )
        .with_disabled(*busy)],
    };
    let on_id = {
        let id = id.clone();
        Callback::from(move |value: String| id.set(value))
    };
    let on_name = {
        let name = name.clone();
        Callback::from(move |value: String| name.set(value))
    };
    let on_url = {
        let url = url.clone();
        Callback::from(move |value: String| url.set(value))
    };
    let on_enabled = {
        let enabled = enabled.clone();
        Callback::from(move |value: bool| enabled.set(value))
    };
    let title_key =
        if editing { "LABEL.PLUGIN_MARKETPLACE_EDIT_SOURCE" } else { "LABEL.PLUGIN_MARKETPLACE_ADD_SOURCE" };
    let content = html! {
        <div class="tp__plugins__source-dialog">
            <h2>{ translate.t(title_key) }</h2>
            if editing {
                <p><span class="tp__plugins__muted">{ translate.t("LABEL.ID") }{ ": " }</span><code>{ (*id).clone() }</code></p>
            } else {
                <Input name="source_id" label={translate.t("LABEL.ID")} value={(*id).clone()} on_change={on_id}
                    required=true />
            }
            <Input name="source_name" label={translate.t("LABEL.NAME")} value={(*name).clone()} on_change={on_name}
                required=true />
            <Input name="source_url" label={translate.t("LABEL.URL")} value={(*url).clone()} on_change={on_url}
                placeholder={Some("https://example.com/marketplace.json".to_owned())} required=true />
            <label class="tp__plugins__source-enabled">
                <ToggleSwitch value={*enabled} aria_label={translate.t("LABEL.ENABLED")} on_change={on_enabled} />
                { " " }{ translate.t("LABEL.ENABLED") }
            </label>
            if let Some(err) = error.as_ref() {
                <div class="tp__plugins__error tp__input-error" role="alert">
                    <strong>{ err.error.clone() }</strong>
                    if !err.problems.is_empty() {
                        <ul>{ for err.problems.iter().map(|problem| html! { <li>{ problem.clone() }</li> }) }</ul>
                    }
                </div>
            }
        </div>
    };
    html! {
        <ContentDialog content={content} actions={actions} close_on_backdrop_click={false}
            close_on_confirm={false} on_confirm={on_result} />
    }
}

#[derive(Properties, Clone, PartialEq)]
pub struct MarketplaceSourcesProps {
    pub sources: Rc<Vec<PluginRepositoryInfoDto>>,
    /// Only users with `SystemWrite` get the mutation controls.
    pub can_write: bool,
    /// A source was added, changed, removed or synced: the lists must reload.
    pub on_changed: Callback<()>,
}

/// The sources of the marketplace: name, url as written, enabled switch, last sync, last error and
/// plugin count, with Sync, Edit and Remove for users who may write.
#[component]
pub fn MarketplaceSources(props: &MarketplaceSourcesProps) -> Html {
    let translate = use_translation();
    let services = use_service_context();
    // `Some(None)` is the add dialog, `Some(Some(source))` the edit dialog.
    let dialog = use_state(|| None::<Option<PluginRepositoryInfoDto>>);
    let remove_for = use_state(|| None::<PluginRepositoryInfoDto>);
    let syncing = use_state(|| None::<String>);
    let acting = use_state(|| false);

    let on_add = {
        let dialog = dialog.clone();
        Callback::from(move |_: String| dialog.set(Some(None)))
    };
    let on_close_dialog = {
        let dialog = dialog.clone();
        Callback::from(move |()| dialog.set(None))
    };
    let on_sync = {
        let services = services.clone();
        let translate = translate.clone();
        let syncing = syncing.clone();
        let on_changed = props.on_changed.clone();
        Callback::from(move |source: PluginRepositoryInfoDto| {
            if syncing.is_some() {
                return;
            }
            syncing.set(Some(source.id.clone()));
            let services = services.clone();
            let translate = translate.clone();
            let syncing = syncing.clone();
            let on_changed = on_changed.clone();
            spawn_local(async move {
                match services.plugins.sync_repository(&source.id).await {
                    Ok(info) => services.toastr.success(format!(
                        "{}: {} ({})",
                        translate.t("MESSAGES.PLUGIN_MARKETPLACE_SYNCED"),
                        info.name,
                        info.plugin_count
                    )),
                    Err(err) => services.toastr.error(err.message()),
                }
                syncing.set(None);
                // a failed sync stores its `last_error`, so the list reloads either way
                on_changed.emit(());
            });
        })
    };
    let on_toggle = {
        let services = services.clone();
        let acting = acting.clone();
        let on_changed = props.on_changed.clone();
        Callback::from(move |(source, enabled): (PluginRepositoryInfoDto, bool)| {
            if *acting {
                return;
            }
            acting.set(true);
            let services = services.clone();
            let acting = acting.clone();
            let on_changed = on_changed.clone();
            spawn_local(async move {
                let body = PluginRepositoryDto { id: source.id.clone(), name: source.name, url: source.url, enabled };
                if let Err(err) = services.plugins.update_repository(&source.id, &body).await {
                    services.toastr.error(err.message());
                }
                acting.set(false);
                on_changed.emit(());
            });
        })
    };
    let on_confirm_remove = {
        let services = services.clone();
        let translate = translate.clone();
        let remove_for = remove_for.clone();
        let acting = acting.clone();
        let on_changed = props.on_changed.clone();
        Callback::from(move |result: DialogResult| {
            let target = (*remove_for).clone();
            remove_for.set(None);
            let (DialogResult::Ok, Some(source)) = (result, target) else {
                return;
            };
            acting.set(true);
            let services = services.clone();
            let translate = translate.clone();
            let acting = acting.clone();
            let on_changed = on_changed.clone();
            spawn_local(async move {
                match services.plugins.remove_repository(&source.id).await {
                    Ok(()) => services.toastr.success(format!(
                        "{}: {}",
                        translate.t("MESSAGES.PLUGIN_MARKETPLACE_SOURCE_REMOVED"),
                        source.name
                    )),
                    Err(err) => services.toastr.error(err.message()),
                }
                acting.set(false);
                on_changed.emit(());
            });
        })
    };

    let render_row = |source: &PluginRepositoryInfoDto| {
        let edit = {
            let dialog = dialog.clone();
            let source = source.clone();
            Callback::from(move |_: String| dialog.set(Some(Some(source.clone()))))
        };
        let remove = {
            let remove_for = remove_for.clone();
            let source = source.clone();
            Callback::from(move |_: String| remove_for.set(Some(source.clone())))
        };
        let sync = {
            let on_sync = on_sync.clone();
            let source = source.clone();
            Callback::from(move |_: String| on_sync.emit(source.clone()))
        };
        let toggle = {
            let on_toggle = on_toggle.clone();
            let source = source.clone();
            Callback::from(move |enabled: bool| on_toggle.emit((source.clone(), enabled)))
        };
        let is_syncing = syncing.as_deref() == Some(source.id.as_str());
        html! {
            <tr key={source.id.clone()}>
                <td>{ source.name.clone() }<br/><code class="tp__plugins__muted">{ source.id.clone() }</code></td>
                <td class="tp__plugins__source-url"><code>{ source.url.clone() }</code></td>
                <td>
                    <ToggleSwitch
                        key={format!("{}-{}", source.id, source.enabled)}
                        value={source.enabled}
                        readonly={!props.can_write || *acting}
                        aria_label={format!("{} {}", translate.t("LABEL.ENABLED"), source.name)}
                        on_change={toggle}
                    />
                </td>
                <td>{ source.last_sync.map_or_else(|| translate.t("LABEL.PLUGIN_MARKETPLACE_NEVER_SYNCED"), format_sync_time) }</td>
                <td>
                    if let Some(last_error) = source.last_error.as_ref() {
                        <span class="tp__plugins__source-error tp__input-error">{ last_error.clone() }</span>
                    }
                </td>
                <td>{ source.plugin_count }</td>
                if props.can_write {
                    <td class="tp__plugins__source-actions">
                        <TextButton
                            name={format!("sync-{}", source.id)}
                            icon="Refresh"
                            title={translate.t(if is_syncing { "LABEL.PLUGIN_MARKETPLACE_SYNCING" } else { "LABEL.PLUGIN_MARKETPLACE_SYNC" })}
                            aria_label={format!("{} {}", translate.t("LABEL.PLUGIN_MARKETPLACE_SYNC"), source.name)}
                            disabled={syncing.is_some() || !source.enabled}
                            onclick={sync}
                        />
                        <TextButton
                            name={format!("edit-source-{}", source.id)}
                            icon="Edit"
                            title={translate.t("LABEL.EDIT")}
                            aria_label={format!("{} {}", translate.t("LABEL.EDIT"), source.name)}
                            disabled={*acting}
                            onclick={edit}
                        />
                        <TextButton
                            name={format!("remove-source-{}", source.id)}
                            icon="Delete"
                            title={translate.t("LABEL.PLUGIN_MARKETPLACE_REMOVE_SOURCE")}
                            aria_label={format!("{} {}", translate.t("LABEL.PLUGIN_MARKETPLACE_REMOVE_SOURCE"), source.name)}
                            disabled={*acting}
                            onclick={remove}
                        />
                    </td>
                }
            </tr>
        }
    };

    html! {
        <section class="tp__plugins__sources">
            <div class="tp__plugins__sources-header">
                <h3>{ translate.t("LABEL.PLUGIN_MARKETPLACE_SOURCES") }</h3>
                if props.can_write {
                    <TextButton name="add-source" icon="Add" title={translate.t("LABEL.PLUGIN_MARKETPLACE_ADD_SOURCE")}
                        disabled={*acting} onclick={on_add} />
                }
            </div>
            if props.sources.is_empty() {
                <p class="tp__plugins__muted">{ translate.t("MESSAGES.PLUGIN_MARKETPLACE_NO_SOURCES") }</p>
            } else {
                <table class="tp__plugins__sources-table">
                    <thead>
                        <tr>
                            <th>{ translate.t("LABEL.NAME") }</th>
                            <th>{ translate.t("LABEL.URL") }</th>
                            <th>{ translate.t("LABEL.ENABLED") }</th>
                            <th>{ translate.t("LABEL.PLUGIN_MARKETPLACE_LAST_SYNC") }</th>
                            <th>{ translate.t("LABEL.PLUGIN_LAST_ERROR") }</th>
                            <th>{ translate.t("LABEL.PLUGINS") }</th>
                            if props.can_write {
                                <th></th>
                            }
                        </tr>
                    </thead>
                    <tbody>{ for props.sources.iter().map(render_row) }</tbody>
                </table>
            }
            if let Some(initial) = (*dialog).clone().filter(|_| props.can_write) {
                <SourceDialog
                    key={initial.as_ref().map_or_else(|| "new".to_owned(), |source| source.id.clone())}
                    initial={initial.clone()}
                    on_saved={props.on_changed.clone()}
                    on_close={on_close_dialog}
                />
            }
            if let Some(source) = (*remove_for).clone() {
                <ConfirmDialog
                    title={format!("{} {}\n{}", translate.t("LABEL.PLUGIN_MARKETPLACE_REMOVE_SOURCE"), source.name, translate.t("MESSAGES.PLUGIN_MARKETPLACE_REMOVE_KEEPS_PLUGINS"))}
                    ok_caption="LABEL.PLUGIN_MARKETPLACE_REMOVE_SOURCE"
                    cancel_caption="LABEL.CANCEL"
                    on_confirm={on_confirm_remove}
                    close_on_backdrop_click=false
                />
            }
        </section>
    }
}
