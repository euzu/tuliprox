//! Marketplace source management: the list of configured sources with their sync state, the add/edit dialog, remove and sync.
use crate::{
    app::components::{
        input::Input, menu_item::MenuItem, popup_menu::PopupMenu, AppIcon, ConfirmDialog, ContentDialog, Table,
        TableDefinition, TextButton, ToggleSwitch,
    },
    hooks::use_service_context,
    html_if,
    i18n::use_translation,
    model::{DialogAction, DialogActions, DialogResult},
    services::PluginActionError,
};
use js_sys::Date;
use shared::model::{PluginRepositoryAddDto, PluginRepositoryDto, PluginRepositoryInfoDto};
use std::rc::Rc;
use wasm_bindgen::JsValue;
use web_sys::MouseEvent;
use yew::{component, html, platform::spawn_local, use_memo, use_state, Callback, Html, Properties, TargetCast};

crate::app::components::define_table_columns! {
    enum MarketplaceSourceColumn {
        Actions => ("actions", "TABLE_COLUMNS.ACTIONS") {
            can_hide: false,
            content: false,
            header_label: Some("LABEL.EMPTY"),
        },
        Name => ("name", "LABEL.NAME") { can_hide: false },
        Url => ("url", "LABEL.URL"),
        Enabled => ("enabled", "LABEL.ENABLED"),
        LastSync => ("last_sync", "LABEL.PLUGIN_MARKETPLACE_LAST_SYNC"),
        LastError => ("last_error", "LABEL.PLUGIN_LAST_ERROR"),
        Plugins => ("plugins", "LABEL.PLUGINS"),
    }
}

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

/// The add/edit dialog of one source. Adding asks for the url only: the id and
/// the name come from the catalog. Editing shows them, and changes the url and
/// the enabled switch. The server's `422`, `409` and fetch problems stay in the dialog.
#[component]
fn SourceDialog(props: &SourceDialogProps) -> Html {
    let translate = use_translation();
    let services = use_service_context();
    let editing = props.initial.is_some();
    let url = use_state(|| props.initial.as_ref().map(|source| source.url.clone()).unwrap_or_default());
    let enabled = use_state(|| props.initial.as_ref().is_none_or(|source| source.enabled));
    let busy = use_state(|| false);
    let error = use_state(|| None::<PluginActionError>);

    let save = {
        let services = services.clone();
        let url = url.clone();
        let enabled = enabled.clone();
        let busy = busy.clone();
        let error = error.clone();
        let initial = props.initial.clone();
        let on_saved = props.on_saved.clone();
        let on_close = props.on_close.clone();
        Callback::from(move |()| {
            if *busy {
                return;
            }
            busy.set(true);
            let url = url.trim().to_owned();
            let initial = initial.clone();
            let enabled = *enabled;
            let services = services.clone();
            let busy = busy.clone();
            let error = error.clone();
            let on_saved = on_saved.clone();
            let on_close = on_close.clone();
            spawn_local(async move {
                let result = match initial {
                    Some(source) => {
                        let body = PluginRepositoryDto { id: source.id.clone(), name: source.name, url, enabled };
                        services.plugins.update_repository(&source.id, &body).await
                    }
                    None => services.plugins.add_repository(&PluginRepositoryAddDto { url }).await,
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
            if let Some(source) = props.initial.as_ref() {
                <p><span class="tp__plugins__muted">{ translate.t("LABEL.ID") }{ ": " }</span><code>{ source.id.clone() }</code></p>
                <p><span class="tp__plugins__muted">{ translate.t("LABEL.NAME") }{ ": " }</span>{ source.name.clone() }</p>
            }
            <Input name="source_url" label={translate.t("LABEL.URL")} value={(*url).clone()} on_change={on_url}
                placeholder={Some("https://example.com/marketplace.json".to_owned())} required=true />
            if editing {
                <label class="tp__plugins__source-enabled">
                    <ToggleSwitch value={*enabled} aria_label={translate.t("LABEL.ENABLED")} on_change={on_enabled} />
                    { " " }{ translate.t("LABEL.ENABLED") }
                </label>
            }
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
    let columns = use_memo((), |()| MarketplaceSourceColumn::columns());
    // `Some(None)` is the add dialog, `Some(Some(source))` the edit dialog.
    let dialog = use_state(|| None::<Option<PluginRepositoryInfoDto>>);
    let remove_for = use_state(|| None::<PluginRepositoryInfoDto>);
    let syncing = use_state(|| None::<String>);
    let acting = use_state(|| false);

    let popup_anchor_ref = use_state(|| None::<web_sys::Element>);
    let popup_is_open = use_state(|| false);
    let selected_source = use_state(|| None::<Rc<PluginRepositoryInfoDto>>);

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

    let handle_popup_close = {
        let popup_is_open = popup_is_open.clone();
        Callback::from(move |()| popup_is_open.set(false))
    };

    let handle_popup_onclick = {
        let set_selected = selected_source.clone();
        let set_anchor = popup_anchor_ref.clone();
        let set_open = popup_is_open.clone();
        Callback::from(move |(dto, event): (Rc<PluginRepositoryInfoDto>, MouseEvent)| {
            if let Some(target) = event.target_dyn_into::<web_sys::Element>() {
                set_selected.set(Some(dto));
                set_anchor.set(Some(target));
                set_open.set(true);
            }
        })
    };

    let handle_menu_click = {
        let popup_is_open = popup_is_open.clone();
        let selected_source = selected_source.clone();
        let dialog = dialog.clone();
        let remove_for = remove_for.clone();
        let on_sync = on_sync.clone();
        let syncing = syncing.clone();
        let can_write = props.can_write;
        Callback::from(move |(name, e): (String, MouseEvent)| {
            e.prevent_default();
            e.stop_propagation();
            if !can_write {
                popup_is_open.set(false);
                return;
            }
            match name.as_str() {
                "edit" => {
                    if let Some(source) = &*selected_source {
                        dialog.set(Some(Some(source.as_ref().clone())));
                    }
                }
                "sync" => {
                    if let Some(source) = &*selected_source {
                        if source.enabled && syncing.is_none() {
                            on_sync.emit(source.as_ref().clone());
                        }
                    }
                }
                "delete" => {
                    if let Some(source) = &*selected_source {
                        remove_for.set(Some(source.as_ref().clone()));
                    }
                }
                _ => {}
            }
            popup_is_open.set(false);
        })
    };

    let render_header_cell = {
        let translate = translate.clone();
        Callback::<usize, Html>::from(move |col| {
            html! {
                if let Some(column) = MarketplaceSourceColumn::from_index(col) {
                    { translate.t(column.header_label()) }
                }
            }
        })
    };

    let on_sort = Callback::<Option<(usize, shared::model::SortOrder)>, ()>::from(|_| {});

    let table_definition = {
        let columns = columns.clone();
        let render_header = render_header_cell.clone();
        let on_sort = on_sort.clone();
        let popup_onclick = handle_popup_onclick.clone();
        let on_toggle = on_toggle.clone();
        let translate = translate.clone();

        use_memo((props.sources.clone(), props.can_write, *acting), move |(sources, can_write, acting)| {
            let items = Some(Rc::new(sources.iter().map(|s| Rc::new(s.clone())).collect::<Vec<_>>()));
            let can_write = *can_write;
            let acting = *acting;
            let popup_onclick = popup_onclick.clone();
            let on_toggle = on_toggle.clone();
            let translate = translate.clone();
            let render_data_cell = Callback::<(usize, usize, Rc<PluginRepositoryInfoDto>), Html>::from(
                move |(_row, col, dto): (usize, usize, Rc<PluginRepositoryInfoDto>)| {
                    match MarketplaceSourceColumn::from_index(col) {
                        Some(MarketplaceSourceColumn::Actions) => {
                            if !can_write {
                                return html! {};
                            }
                            let popup_onclick = popup_onclick.clone();
                            html! {
                                <button class="tp__icon-button"
                                    onclick={Callback::from(move |event: MouseEvent| popup_onclick.emit((dto.clone(), event)))}>
                                    <AppIcon name="Popup" />
                                </button>
                            }
                        }
                        Some(MarketplaceSourceColumn::Name) => {
                            html! {
                                <>
                                    { dto.name.clone() }
                                    <br/>
                                    <code class="tp__plugins__muted">{ dto.id.clone() }</code>
                                </>
                            }
                        }
                        Some(MarketplaceSourceColumn::Url) => {
                            html! {
                                <code class="tp__plugins__source-url">{ dto.url.clone() }</code>
                            }
                        }
                        Some(MarketplaceSourceColumn::Enabled) => {
                            let toggle = {
                                let on_toggle = on_toggle.clone();
                                let source = dto.as_ref().clone();
                                Callback::from(move |enabled: bool| on_toggle.emit((source.clone(), enabled)))
                            };
                            html! {
                                <ToggleSwitch
                                    key={format!("{}-{}", dto.id, dto.enabled)}
                                    value={dto.enabled}
                                    readonly={!can_write || acting}
                                    aria_label={format!("{} {}", translate.t("LABEL.ENABLED"), dto.name)}
                                    on_change={toggle}
                                />
                            }
                        }
                        Some(MarketplaceSourceColumn::LastSync) => {
                            html! {
                                { dto.last_sync.map_or_else(|| translate.t("LABEL.PLUGIN_MARKETPLACE_NEVER_SYNCED"), format_sync_time) }
                            }
                        }
                        Some(MarketplaceSourceColumn::LastError) => {
                            html! {
                                if let Some(last_error) = dto.last_error.as_ref() {
                                    <span class="tp__plugins__source-error tp__input-error">{ last_error.clone() }</span>
                                }
                            }
                        }
                        Some(MarketplaceSourceColumn::Plugins) => html! { dto.plugin_count },
                        _ => html! {},
                    }
                },
            );

            TableDefinition::<PluginRepositoryInfoDto> {
                table_id: "plugins.marketplace_sources".into(),
                columns: columns.clone(),
                row_key: Callback::from(|(_, item): (usize, Rc<PluginRepositoryInfoDto>)| item.id.clone().into()),
                items,
                on_sort,
                render_header_cell: render_header,
                render_data_cell,
            }
        })
    };

    let is_selected_syncing = selected_source.as_ref().is_some_and(|s| syncing.as_deref() == Some(s.id.as_str()));
    let sync_label = translate.t(if is_selected_syncing {
        "LABEL.PLUGIN_MARKETPLACE_SYNCING"
    } else {
        "LABEL.PLUGIN_MARKETPLACE_SYNC"
    });

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
                <Table::<PluginRepositoryInfoDto> definition={table_definition} />
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
            <PopupMenu is_open={*popup_is_open} anchor_ref={(*popup_anchor_ref).clone()} on_close={handle_popup_close}>
                { html_if!(props.can_write, {
                    <>
                        <MenuItem icon="Edit" name="edit" label={translate.t("LABEL.EDIT")} onclick={&handle_menu_click} />
                        { html_if!(selected_source.as_ref().is_some_and(|s| s.enabled), {
                            <MenuItem icon="Refresh" name="sync" label={sync_label} onclick={&handle_menu_click} />
                        })}
                        <hr/>
                        <MenuItem icon="Delete" name="delete" label={translate.t("LABEL.PLUGIN_MARKETPLACE_REMOVE_SOURCE")} onclick={&handle_menu_click} class="tp__delete_action" />
                    </>
                })}
            </PopupMenu>
        </section>
    }
}
