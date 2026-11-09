use crate::{
    app::{
        components::{
            custom_dialog::CustomDialog,
            define_table_columns,
            table::{PagedTable, TableDefinition},
            Checkbox, DropDownOption, DropDownSelection, IconButton, NoContent, Select, TextButton,
        },
        context::ConfigContext,
    },
    config_field_child,
    hooks::use_service_context,
    i18n::use_translation,
};
use shared::model::{
    OrphanedOverrideFileDto, OverrideDiscardItemDto, OverrideDiscardRequest, OverrideEntryState, OverrideItemKind,
    OverrideOrphanEntryDto, OverrideOrphansDto, OverrideScopeDto,
};
use std::{
    collections::{HashMap, HashSet},
    rc::Rc,
};
use web_sys::MouseEvent;
use yew::{create_portal, platform::spawn_local, prelude::*};

fn portal_to_body(content: Html) -> Html {
    if let Some(body) = web_sys::window().and_then(|w| w.document()).and_then(|d| d.body()) {
        create_portal(content, body.into())
    } else {
        content
    }
}

define_table_columns! {
    enum OrphanTableColumn {
        // Row actions come first with an empty header (project rule).
        Actions => ("actions", "") { can_hide: false },
        Select => ("select", "") { can_hide: false },
        Scope => ("scope", "LABEL.SCOPE"),
        File => ("file", "LABEL.FILE"),
        Kind => ("kind", "LABEL.TYPE"),
        Id => ("id", "LABEL.ID"),
        Hint => ("hint", "LABEL.HINT"),
        State => ("state", "LABEL.STATUS"),
    }
}

/// Translation key of the override item kind. The enum's `Debug` output is not user-facing text.
#[must_use]
fn kind_label_key(kind: OverrideItemKind) -> &'static str {
    match kind {
        OverrideItemKind::Live => "LABEL.LIVE",
        OverrideItemKind::Vod => "LABEL.VOD",
        OverrideItemKind::Series => "LABEL.SERIES",
    }
}

fn entry_key(entry: &OverrideOrphanEntryDto) -> String {
    format!("{}:{}:{:?}:{}", entry.scope, entry.file, entry.kind, entry.id)
}

#[derive(Properties, PartialEq, Clone)]
pub struct ExplorerOrphansViewProps {
    #[prop_or_default]
    pub input: Option<String>,
    pub on_close: Callback<()>,
}

#[component]
pub fn ExplorerOrphansView(props: &ExplorerOrphansViewProps) -> Html {
    let translate = use_translation();
    let services = use_service_context();
    let config_ctx = use_context::<ConfigContext>();

    let inputs = use_memo(config_ctx.clone(), |ctx| {
        let mut list = Vec::new();
        if let Some(cfg) = ctx.as_ref().and_then(|c| c.config.as_ref()) {
            for inp in &cfg.sources.inputs {
                list.push(inp.name.clone());
            }
        }
        list
    });

    let current_input =
        use_state(|| props.input.clone().or_else(|| inputs.first().map(ToString::to_string)).unwrap_or_default());

    let orphans_data = use_state(|| None::<OverrideOrphansDto>);
    let is_loading = use_state(|| false);
    let selected_keys = use_state(HashSet::<String>::new);
    let page = use_state(|| 1u32);
    let page_size = use_state(|| 25u16);

    let load_orphans = {
        let services = services.clone();
        let translate = translate.clone();
        let orphans_data = orphans_data.clone();
        let is_loading = is_loading.clone();
        let selected_keys = selected_keys.clone();
        let current_input = (*current_input).clone();

        Callback::from(move |()| {
            let services = services.clone();
            let translate = translate.clone();
            let orphans_data = orphans_data.clone();
            let is_loading = is_loading.clone();
            let selected_keys = selected_keys.clone();
            let inp = current_input.clone();

            is_loading.set(true);
            spawn_local(async move {
                match services.playlist.get_orphans(Some(&inp)).await {
                    Ok(Some(data)) => {
                        // Default selection: select only orphaned entries
                        let mut initial_selected = HashSet::new();
                        for entry in &data.entries {
                            if entry.state == OverrideEntryState::Orphaned {
                                initial_selected.insert(entry_key(entry));
                            }
                        }
                        selected_keys.set(initial_selected);
                        orphans_data.set(Some(data));
                    }
                    Ok(None) => orphans_data.set(None),
                    Err(err) => {
                        orphans_data.set(None);
                        services.toastr.error(
                            translate
                                .t("MESSAGES.PLAYLIST_EXPLORER.ORPHANS_LOAD_FAILED")
                                .replace("{error}", &err.to_string()),
                        );
                    }
                }
                is_loading.set(false);
            });
        })
    };

    // Load orphans whenever selected input changes
    {
        let load_orphans = load_orphans.clone();
        let inp = (*current_input).clone();
        use_effect_with(inp, move |_| {
            load_orphans.emit(());
            || ()
        });
    }

    let entries: Vec<Rc<OverrideOrphanEntryDto>> =
        orphans_data.as_ref().map(|d| d.entries.iter().cloned().map(Rc::new).collect()).unwrap_or_default();

    let orphaned_files: Vec<OrphanedOverrideFileDto> =
        orphans_data.as_ref().map(|d| d.orphaned_files.clone()).unwrap_or_default();

    let single_discard = {
        let services = services.clone();
        let translate = translate.clone();
        let load_orphans = load_orphans.clone();

        Callback::from(move |entry: OverrideOrphanEntryDto| {
            let services = services.clone();
            let translate = translate.clone();
            let load_orphans = load_orphans.clone();

            spawn_local(async move {
                // Fetch file revision; without it the discard would be a blind write.
                let rev = match services.playlist.get_overrides(entry.scope, &entry.file).await {
                    Ok(Some(file)) => file.revision,
                    Ok(None) => String::new(),
                    Err(err) => {
                        services.toastr.error(err.to_string());
                        return;
                    }
                };

                let req = OverrideDiscardRequest {
                    scope: entry.scope,
                    name: entry.file,
                    base_revision: rev,
                    items: vec![OverrideDiscardItemDto { input: entry.input, kind: entry.kind, id: entry.id }],
                };

                match services.playlist.discard_overrides(&req).await {
                    Ok(_) => {
                        services.toastr.info(translate.t("LABEL.EXPLORER_DISCARD_SUCCESS"));
                        load_orphans.emit(());
                    }
                    Err(crate::error::Error::Conflict(_)) => {
                        services.toastr.warning(translate.t("MESSAGES.PLAYLIST_EXPLORER.SAVE_CONFLICT"));
                        load_orphans.emit(());
                    }
                    Err(e) => {
                        services.toastr.error(e.to_string());
                    }
                }
            });
        })
    };

    let bulk_discard = {
        let services = services.clone();
        let translate = translate.clone();
        let load_orphans = load_orphans.clone();
        let entries = entries.clone();
        let selected_keys = selected_keys.clone();

        Callback::from(move |_: String| {
            let services = services.clone();
            let translate = translate.clone();
            let load_orphans = load_orphans.clone();
            let selected: HashSet<String> = (*selected_keys).clone();

            let items_to_discard: Vec<OverrideOrphanEntryDto> =
                entries.iter().filter(|e| selected.contains(&entry_key(e))).map(|e| (**e).clone()).collect();

            if items_to_discard.is_empty() {
                return;
            }

            spawn_local(async move {
                // Group by (scope, file)
                let mut grouped: HashMap<(OverrideScopeDto, String), Vec<OverrideDiscardItemDto>> = HashMap::new();
                for it in items_to_discard {
                    grouped.entry((it.scope, it.file)).or_default().push(OverrideDiscardItemDto {
                        input: it.input,
                        kind: it.kind,
                        id: it.id,
                    });
                }

                let mut had_conflict = false;
                for ((scope, file), items) in grouped {
                    let rev = match services.playlist.get_overrides(scope, &file).await {
                        Ok(Some(f)) => f.revision,
                        Ok(None) => String::new(),
                        Err(err) => {
                            services.toastr.error(err.to_string());
                            continue;
                        }
                    };

                    let req = OverrideDiscardRequest { scope, name: file, base_revision: rev, items };

                    match services.playlist.discard_overrides(&req).await {
                        Ok(_) => {}
                        Err(crate::error::Error::Conflict(_)) => {
                            had_conflict = true;
                        }
                        Err(e) => {
                            services.toastr.error(e.to_string());
                        }
                    }
                }

                if had_conflict {
                    services.toastr.warning(translate.t("MESSAGES.PLAYLIST_EXPLORER.SAVE_CONFLICT"));
                } else {
                    services.toastr.info(translate.t("LABEL.EXPLORER_DISCARD_SUCCESS"));
                }
                load_orphans.emit(());
            });
        })
    };

    let delete_file = {
        let services = services.clone();
        let translate = translate.clone();
        let load_orphans = load_orphans.clone();

        Callback::from(move |file: OrphanedOverrideFileDto| {
            let services = services.clone();
            let translate = translate.clone();
            let load_orphans = load_orphans.clone();

            spawn_local(async move {
                let req = OverrideDiscardRequest {
                    scope: file.scope,
                    name: file.name,
                    base_revision: file.revision,
                    items: vec![], // Empty items deletes whole orphaned file
                };

                match services.playlist.discard_overrides(&req).await {
                    Ok(_) => {
                        services.toastr.info(translate.t("LABEL.EXPLORER_DISCARD_SUCCESS"));
                        load_orphans.emit(());
                    }
                    Err(crate::error::Error::Conflict(_)) => {
                        services.toastr.warning(translate.t("MESSAGES.PLAYLIST_EXPLORER.SAVE_CONFLICT"));
                        load_orphans.emit(());
                    }
                    Err(e) => {
                        services.toastr.error(e.to_string());
                    }
                }
            });
        })
    };

    let columns = use_memo((), |()| OrphanTableColumn::columns());

    let total_items = entries.len();
    let current_page_size = usize::from(*page_size);
    let total_pages = ((total_items + current_page_size.saturating_sub(1)) / current_page_size.max(1)).max(1) as u32;

    // The list shrinks when entries are discarded; the page must follow.
    let current_page = (*page).clamp(1, total_pages);
    let start_idx = ((current_page.saturating_sub(1) as usize) * current_page_size).min(total_items);
    let end_idx = (start_idx + current_page_size).min(total_items);
    let paged_items: Vec<Rc<OverrideOrphanEntryDto>> = entries[start_idx..end_idx].to_vec();

    let render_header_cell = {
        let translate = translate.clone();
        let paged_items = paged_items.clone();
        let selected_keys = selected_keys.clone();

        Callback::<usize, Html>::from(move |col: usize| {
            match OrphanTableColumn::from_index(col) {
                // The row actions column carries no header text by project rule.
                Some(OrphanTableColumn::Actions) => html! {},
                Some(OrphanTableColumn::Select) => {
                    // Select-all on the current page (only orphaned entries by default).
                    let all_orphaned_selected = !paged_items.is_empty()
                        && paged_items
                            .iter()
                            .all(|e| e.state != OverrideEntryState::Orphaned || selected_keys.contains(&entry_key(e)));

                    let paged = paged_items.clone();
                    let selected_keys = selected_keys.clone();
                    let on_toggle_all = Callback::from(move |e: Event| {
                        let Some(input_el) = e.target_dyn_into::<web_sys::HtmlInputElement>() else {
                            return;
                        };
                        let checked = input_el.checked();
                        let mut current = (*selected_keys).clone();
                        for it in &paged {
                            let k = entry_key(it);
                            if checked {
                                if it.state == OverrideEntryState::Orphaned {
                                    current.insert(k);
                                }
                            } else {
                                current.remove(&k);
                            }
                        }
                        selected_keys.set(current);
                    });

                    let select_label = translate.t("LABEL.EXPLORER_SELECT_PAGE");
                    html! {
                        <Checkbox aria_label={Some(select_label)} checked={all_orphaned_selected} onchange={on_toggle_all} />
                    }
                }
                Some(col_enum) => html! { <span>{ translate.t(col_enum.header_label()) }</span> },
                None => html! {},
            }
        })
    };

    let render_data_cell = {
        let translate = translate.clone();
        let selected_keys = selected_keys.clone();
        let single_discard = single_discard.clone();

        Callback::<(usize, usize, Rc<OverrideOrphanEntryDto>), Html>::from(
            move |(_row, col, item): (usize, usize, Rc<OverrideOrphanEntryDto>)| {
                let entry = item.clone();
                let k = entry_key(&entry);
                let is_checked = selected_keys.contains(&k);

                match OrphanTableColumn::from_index(col) {
                    Some(OrphanTableColumn::Select) => {
                        let selected_keys = selected_keys.clone();
                        let k = k.clone();
                        let select_label = translate.t("LABEL.EXPLORER_SELECTED_ITEMS");
                        let on_toggle = Callback::from(move |e: Event| {
                            let Some(input_el) = e.target_dyn_into::<web_sys::HtmlInputElement>() else {
                                return;
                            };
                            let checked = input_el.checked();
                            let mut set = (*selected_keys).clone();
                            if checked {
                                set.insert(k.clone());
                            } else {
                                set.remove(&k);
                            }
                            selected_keys.set(set);
                        });
                        html! {
                            <Checkbox aria_label={Some(select_label)} checked={is_checked} onchange={on_toggle} />
                        }
                    }
                    Some(OrphanTableColumn::Scope) => {
                        let scope_str = entry.scope.as_str();
                        html! { <span class="tp__badge">{ scope_str }</span> }
                    }
                    Some(OrphanTableColumn::File) => {
                        let file = entry.file.clone();
                        html! { <span class="tp__ellipsis-cell tp__orphans-file" title={file.clone()}>{ file }</span> }
                    }
                    Some(OrphanTableColumn::Kind) => {
                        html! { <span class="tp__badge">{ translate.t(kind_label_key(entry.kind)) }</span> }
                    }
                    Some(OrphanTableColumn::Id) => {
                        let id = entry.id.clone();
                        html! { <code class="tp__code tp__ellipsis-cell tp__orphans-id" title={id.clone()}>{ id }</code> }
                    }
                    Some(OrphanTableColumn::Hint) => {
                        let hint = entry.hint.clone().unwrap_or_default();
                        html! { <span class="tp__hint-text tp__ellipsis-cell tp__orphans-hint" title={hint.clone()}>{ hint }</span> }
                    }
                    Some(OrphanTableColumn::State) => {
                        let (badge_class, text) = match entry.state {
                            OverrideEntryState::Active => ("badge-success", translate.t("LABEL.EXPLORER_STATE_ACTIVE")),
                            OverrideEntryState::Orphaned => {
                                ("badge-danger", translate.t("LABEL.EXPLORER_STATE_ORPHANED"))
                            }
                            OverrideEntryState::Unknown => {
                                ("badge-warning", translate.t("LABEL.EXPLORER_STATE_UNKNOWN"))
                            }
                        };
                        html! {
                            <span class={format!("tp__badge {}", badge_class)}>
                                { text }
                            </span>
                        }
                    }
                    Some(OrphanTableColumn::Actions) => {
                        let single_discard = single_discard.clone();
                        let it = (*entry).clone();
                        let on_discard = Callback::from(move |_: (String, MouseEvent)| {
                            single_discard.emit(it.clone());
                        });
                        html! {
                            <IconButton
                                name="discard"
                                icon="Trash"
                                hint={translate.t("LABEL.EXPLORER_DISCARD")}
                                aria_label={Some(translate.t("LABEL.EXPLORER_DISCARD"))}
                                onclick={on_discard}
                            />
                        }
                    }
                    None => html! {},
                }
            },
        )
    };

    let table_definition = {
        let columns = columns.clone();
        let paged_rc = Rc::new(paged_items);

        Rc::new(TableDefinition::<OverrideOrphanEntryDto> {
            table_id: "playlist.explorer.orphans".into(),
            columns,
            row_key: Callback::from(|(_, item): (usize, Rc<OverrideOrphanEntryDto>)| entry_key(&item).into()),
            items: Some(paged_rc),
            on_sort: Callback::noop(),
            render_header_cell,
            render_data_cell,
        })
    };

    let input_options: Rc<Vec<DropDownOption>> =
        use_memo((inputs.clone(), (*current_input).clone()), |(inputs, current)| {
            inputs
                .iter()
                .map(|inp| {
                    DropDownOption::new(inp.as_ref(), html! { inp.to_string() }, inp.as_ref() == current.as_str())
                })
                .collect()
        });

    let on_input_select = {
        let current_input = current_input.clone();
        Callback::from(move |(_, selection): (String, DropDownSelection)| {
            if let DropDownSelection::Single(val) = selection {
                current_input.set(val);
            }
        })
    };

    let select_all_orphaned = {
        let entries = entries.clone();
        let selected_keys = selected_keys.clone();
        Callback::from(move |_: String| {
            let mut keys = HashSet::new();
            for e in &entries {
                if e.state == OverrideEntryState::Orphaned {
                    keys.insert(entry_key(e));
                }
            }
            selected_keys.set(keys);
        })
    };

    let on_close_dialog = {
        let on_close = props.on_close.clone();
        Callback::from(move |()| on_close.emit(()))
    };

    portal_to_body(html! {
        <CustomDialog
            open={true}
            modal={true}
            class={Some("tp__orphans-dialog".to_string())}
            on_close={Some(on_close_dialog)}
        >
            <h2>{ translate.t("LABEL.EXPLORER_OVERRIDE_CLEANUP") }</h2>
            <div class="tp__dialog__body tp__orphans-dialog__body">
                <div class="tp__orphans-input-selector">
                    { config_field_child!(translate.t("LABEL.INPUT"), "PLAYLIST_OVERRIDE.INPUT", {
                        html! {
                            <Select
                                name="orphans_input"
                                multi_select={false}
                                required={true}
                                on_select={on_input_select}
                                options={input_options.clone()}
                            />
                        }
                    }) }
                </div>

                <div class="tp__orphans-toolbar">
                    <TextButton
                        name="select_orphaned"
                        icon="SelectAll"
                        title={translate.t("LABEL.EXPLORER_SELECT_ORPHANED")}
                        onclick={select_all_orphaned}
                    />
                    <TextButton
                        class="secondary"
                        name="discard_selected"
                        icon="Trash"
                        title={format!("{} ({})", translate.t("LABEL.EXPLORER_DISCARD_SELECTED"), selected_keys.len())}
                        disabled={selected_keys.is_empty()}
                        onclick={bulk_discard}
                    />
                </div>

                <div class="tp__orphans-table-wrap">
                    {
                        if *is_loading {
                            html! { <div class="tp__loading">{ translate.t("LABEL.LOADING") }</div> }
                        } else if entries.is_empty() && orphaned_files.is_empty() {
                            html! { <NoContent text={translate.t("LABEL.EXPLORER_NO_ORPHANS")} /> }
                        } else {
                            html! {
                                <PagedTable<OverrideOrphanEntryDto>
                                    definition={table_definition}
                                    page={current_page}
                                    page_size={*page_size}
                                    total_items={total_items as u64}
                                    total_pages={total_pages}
                                    has_prev={current_page > 1}
                                    has_next={current_page < total_pages}
                                    on_page_change={let page = page.clone(); Callback::from(move |p| page.set(p))}
                                    on_page_size_change={let page_size = page_size.clone(); Callback::from(move |s| page_size.set(s))}
                                />
                            }
                        }
                    }
                </div>

                {
                    if orphaned_files.is_empty() {
                        html! {}
                    } else {
                        html! {
                            <div class="tp__orphans-files-section">
                                <h3>{ translate.t("LABEL.EXPLORER_ORPHANED_FILES") }</h3>
                                <div class="tp__orphans-files-list">
                                    {
                                        for orphaned_files.into_iter().map(|file| {
                                            let delete_file = delete_file.clone();
                                            let f = file.clone();
                                            let on_del = Callback::from(move |_: String| {
                                                delete_file.emit(f.clone());
                                            });
                                            html! {
                                                <div key={format!("{}:{}", file.scope, file.name)} class="tp__orphans-file-row">
                                                    <span class="tp__orphaned-file-name">
                                                        { translate.t("LABEL.EXPLORER_FILE_SCOPE")
                                                            .replace("{file}", &file.name)
                                                            .replace("{scope}", file.scope.as_str()) }
                                                    </span>
                                                    <TextButton
                                                        name="delete_file"
                                                        class="secondary"
                                                        icon="Delete"
                                                        title={translate.t("LABEL.EXPLORER_DELETE_FILE")}
                                                        onclick={on_del}
                                                    />
                                                </div>
                                            }
                                        })
                                    }
                                </div>
                            </div>
                        }
                    }
                }
            </div>

            <div class="tp__dialog__toolbar">
                <div class="tp__dialog__toolbar-right">
                    <TextButton
                        class="secondary"
                        name="close_orphans"
                        icon="Cancel"
                        title={translate.t("LABEL.CLOSE")}
                        onclick={let on_close = props.on_close.clone(); Callback::from(move |_: String| on_close.emit(()))}
                    />
                </div>
            </div>
        </CustomDialog>
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn orphan_file_classes_match_stylesheet() {
        let source = include_str!("orphans_view.rs").split("#[cfg(test)]").next().unwrap();
        let styles = include_str!("../../../../../scss/app/components/playlist/_playlist_explorer.scss");
        for class in ["tp__orphans-files-section", "tp__orphans-files-list", "tp__orphans-file-row"] {
            assert!(source.contains(&format!("class=\"{class}\"")), "component does not use {class}");
            assert!(styles.contains(&format!(".{class}")), "stylesheet does not define .{class}");
        }
    }
}
