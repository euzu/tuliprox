use crate::{
    app::components::{
        playlist::explorer::{
            identity::Id,
            sort::{epg_badge_label_key, is_edited, item_chno},
        },
        table::{PagedTable, TableDefinition},
        Checkbox, IconButton,
    },
    i18n::use_translation,
};
use shared::model::UiPlaylistItem;
use std::rc::Rc;
use wasm_bindgen::JsCast;
use yew::prelude::*;

crate::app::components::define_table_columns! {
    enum LiveTableColumn {
        Actions => ("actions", "TABLE_COLUMNS.ACTIONS") { can_hide: false, content: false },
        Select => ("select", "TABLE_COLUMNS.SELECT") { can_hide: false, content: false },
        ChNo => ("chno", "LABEL.EXPLORER_SORT_CHANNEL_NO") { header_label: Some("LABEL.EXPLORER_CHNO_SHORT") },
        Logo => ("logo", "LABEL.LOGO"),
        Name => ("name", "LABEL.NAME") { can_hide: false },
        Epg => ("epg", "LABEL.EPG"),
        Badges => ("badges", "LABEL.STATUS"),
    }
}

impl LiveTableColumn {
    pub fn columns_for_viewport(is_mobile: bool) -> Vec<crate::app::components::TableColumn> {
        Self::ALL
            .iter()
            .map(|col| {
                let mut meta = col.definition().metadata();
                if is_mobile && matches!(col, Self::Logo | Self::Badges) {
                    meta.default_visible = false;
                }
                meta
            })
            .collect()
    }
}

#[derive(Properties, PartialEq, Clone)]
pub struct ExplorerLiveViewProps {
    /// `Id` compares by pointer, so the prop check never walks the filtered catalog.
    pub items: Id<Vec<Rc<UiPlaylistItem>>>,
    pub selected_item: Option<Id<UiPlaylistItem>>,
    pub on_select_item: Callback<Rc<UiPlaylistItem>>,
    pub on_popup_menu: Callback<(Rc<UiPlaylistItem>, MouseEvent)>,
    #[prop_or_default]
    pub selected_ids: Rc<std::collections::HashSet<u32>>,
    #[prop_or_default]
    pub on_toggle_select: Callback<u32>,
    #[prop_or_default]
    pub on_select_page: Callback<(Vec<u32>, bool)>,
    #[prop_or_default]
    pub is_mobile: bool,
}

#[component]
pub fn ExplorerLiveView(props: &ExplorerLiveViewProps) -> Html {
    let translate = use_translation();
    let page = use_state(|| 1u32);
    let page_size = use_state(|| 50u16);

    let columns = use_memo(props.is_mobile, |mobile| LiveTableColumn::columns_for_viewport(*mobile));

    let total_items = props.items.len();
    let current_page_size = usize::from(*page_size).max(1);
    let total_pages = total_items.div_ceil(current_page_size).max(1) as u32;
    // A search or filter can shrink the list below the current page.
    let current_page = (*page).clamp(1, total_pages);

    let start_idx = ((current_page.saturating_sub(1) as usize) * current_page_size).min(total_items);
    let end_idx = (start_idx + current_page_size).min(total_items);

    let render_header_cell = {
        let translate = translate.clone();
        let items = props.items.clone();
        let selected_ids = props.selected_ids.clone();
        let on_select_page = props.on_select_page.clone();

        Callback::<usize, Html>::from(move |col: usize| match LiveTableColumn::from_index(col) {
            // The row actions column carries no header text by project rule.
            Some(LiveTableColumn::Actions) => html! {},
            Some(LiveTableColumn::Select) => {
                let page_items = &items[start_idx..end_idx];
                let all_selected =
                    !page_items.is_empty() && page_items.iter().all(|it| selected_ids.contains(&it.virtual_id));
                let page_vids: Vec<u32> = page_items.iter().map(|it| it.virtual_id).collect();
                let on_select_page = on_select_page.clone();
                let onchange = Callback::from(move |e: Event| {
                    if let Some(target) = e.target_dyn_into::<web_sys::HtmlInputElement>() {
                        on_select_page.emit((page_vids.clone(), target.checked()));
                    }
                });
                let select_label = translate.t("LABEL.EXPLORER_SELECT_PAGE");
                html! {
                    <span class="row-cell-select" onclick={Callback::from(|e: MouseEvent| e.stop_propagation())}>
                        <Checkbox aria_label={Some(select_label)} checked={all_selected} onchange={onchange} />
                    </span>
                }
            }
            Some(col_enum) => html! { <span>{ translate.t(col_enum.header_label()) }</span> },
            None => html! {},
        })
    };

    let render_data_cell = {
        let translate = translate.clone();
        let on_popup_menu = props.on_popup_menu.clone();
        let on_select_item = props.on_select_item.clone();
        let selected_ids = props.selected_ids.clone();
        let on_toggle_select = props.on_toggle_select.clone();

        Callback::<(usize, usize, Rc<UiPlaylistItem>), Html>::from(
            move |(_row, col, item): (usize, usize, Rc<UiPlaylistItem>)| {
                let pli = item.clone();
                match LiveTableColumn::from_index(col) {
                    Some(LiveTableColumn::Actions) => {
                        let pli_for_popup = pli.clone();
                        let popup_cb = on_popup_menu.clone();
                        let on_popup_click = Callback::from(move |(_, e): (String, MouseEvent)| {
                            popup_cb.emit((pli_for_popup.clone(), e));
                        });
                        html! {
                            <IconButton
                                name="row_actions"
                                icon="Popup"
                                aria_label={Some(translate.t("LABEL.ACTIONS"))}
                                onclick={on_popup_click}
                            />
                        }
                    }
                    Some(LiveTableColumn::Select) => {
                        let vid = pli.virtual_id;
                        let is_checked = selected_ids.contains(&vid);
                        let on_toggle_select = on_toggle_select.clone();
                        let onchange = Callback::from(move |e: Event| {
                            e.stop_propagation();
                            on_toggle_select.emit(vid);
                        });
                        let select_label = translate.t("LABEL.EXPLORER_SELECTED_ITEMS");
                        html! {
                            <span class="row-cell-select" onclick={Callback::from(|e: MouseEvent| e.stop_propagation())}>
                                <Checkbox aria_label={Some(select_label)} checked={is_checked} onchange={onchange} />
                            </span>
                        }
                    }
                    Some(LiveTableColumn::ChNo) => {
                        let chno = item_chno(&pli).map(|n| n.to_string()).unwrap_or_else(|| "-".to_string());
                        html! { <span class="row-cell-chno chno">{ chno }</span> }
                    }
                    Some(LiveTableColumn::Logo) => {
                        let logo_src = if pli.logo.is_empty() {
                            "assets/missing-logo.svg".to_string()
                        } else {
                            pli.logo.to_string()
                        };
                        let alt =
                            if pli.title.is_empty() { translate.t("LABEL.CHANNEL") } else { pli.title.to_string() };
                        html! {
                            <span class="row-cell-logo">
                                <span class="logo">
                                    <img alt={alt} src={logo_src} loading="lazy"
                                        onerror={Callback::from(|e: web_sys::Event| {
                                            if let Some(target) = e.target() {
                                                if let Ok(img) = target.dyn_into::<web_sys::HtmlImageElement>() {
                                                    img.set_src("assets/missing-logo.svg");
                                                }
                                            }
                                        })}
                                    />
                                </span>
                            </span>
                        }
                    }
                    Some(LiveTableColumn::Name) => {
                        let epg_id_text = match &pli.epg_channel_id {
                            Some(id) if !id.trim().is_empty() => {
                                html! { <small>{ id.as_ref() }</small> }
                            }
                            _ => html! {},
                        };
                        let item_click = {
                            let on_select = on_select_item.clone();
                            let pli_clone = pli.clone();
                            Callback::from(move |_: MouseEvent| on_select.emit(pli_clone.clone()))
                        };
                        html! {
                            <button type="button" class="row-cell-nm nm" onclick={item_click}>
                                <b>{ pli.title.as_ref() }</b>
                                { epg_id_text }
                            </button>
                        }
                    }
                    Some(LiveTableColumn::Epg) => {
                        let epg_badge = match epg_badge_label_key(pli.epg_state()) {
                            Some(label_key) => {
                                let label = translate.t(label_key);
                                html! {
                                    <span class="noepg" title={label.clone()} aria-label={label.clone()}>
                                        <span class="noepg-icon">{ "⚠" }</span>
                                        <span class="noepg-text">{ format!(" {label}") }</span>
                                    </span>
                                }
                            }
                            None => html! {},
                        };
                        html! { <span class="row-cell-epg epg">{ epg_badge }</span> }
                    }
                    Some(LiveTableColumn::Badges) => {
                        html! {
                            <span class="row-cell-badges badges">
                                {
                                    if is_edited(&pli) {
                                        html! { <span class="ed">{ "✎ " }{ translate.t("LABEL.EXPLORER_EDITED_BADGE") }</span> }
                                    } else {
                                        html! {}
                                    }
                                }
                                {
                                    if pli.override_state().is_some_and(|s| s.pending) {
                                        html! { <span class="pending" title={translate.t("LABEL.EXPLORER_PENDING_UNTIL_UPDATE")}>{ "⏳ " }{ translate.t("LABEL.EXPLORER_PENDING_BADGE") }</span> }
                                    } else {
                                        html! {}
                                    }
                                }
                            </span>
                        }
                    }
                    None => html! {},
                }
            },
        )
    };

    let table_definition = {
        let columns = columns.clone();
        let paged_items: Vec<Rc<UiPlaylistItem>> = props.items[start_idx..end_idx].to_vec();

        Rc::new(TableDefinition::<UiPlaylistItem> {
            // Mobile keeps its own column layout, so hiding Logo/Status on a phone never rewrites the
            // desktop layout (and vice versa).
            table_id: if props.is_mobile { "playlist.explorer.live.mobile" } else { "playlist.explorer.live" }.into(),
            columns,
            row_key: Callback::from(|(_, item): (usize, Rc<UiPlaylistItem>)| item.virtual_id.to_string().into()),
            items: Some(Rc::new(paged_items)),
            on_sort: Callback::noop(),
            render_header_cell,
            render_data_cell,
        })
    };

    let handle_page_change = {
        let page = page.clone();
        Callback::from(move |new_page: u32| page.set(new_page))
    };

    let handle_page_size_change = {
        let page_size = page_size.clone();
        let page = page.clone();
        Callback::from(move |new_size: u16| {
            page_size.set(new_size.max(1));
            page.set(1);
        })
    };

    html! {
        <div class="live-table">
            <PagedTable<UiPlaylistItem>
                definition={table_definition}
                page={current_page}
                page_size={*page_size}
                total_items={total_items as u64}
                total_pages={total_pages}
                has_prev={current_page > 1}
                has_next={current_page < total_pages}
                on_page_change={handle_page_change}
                on_page_size_change={handle_page_size_change}
                on_row_click={Some(props.on_select_item.clone())}
                selected_row_key={props.selected_item.as_ref().map(|item| AttrValue::from(item.virtual_id.to_string()))}
            />
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn columns_for_viewport_hides_logo_and_badges_on_mobile_by_default() {
        let desktop = LiveTableColumn::columns_for_viewport(false);
        let mobile = LiveTableColumn::columns_for_viewport(true);

        for col in &desktop {
            assert!(col.default_visible, "all columns should be visible by default on desktop: {}", col.id);
        }

        for col in &mobile {
            if col.id == "logo" || col.id == "badges" {
                assert!(!col.default_visible, "column {} should be hidden by default on mobile", col.id);
                assert!(col.can_hide, "column {} must be hideable/toggleable in columns panel", col.id);
            } else {
                assert!(col.default_visible, "column {} should be visible by default on mobile", col.id);
            }
        }
    }
}
