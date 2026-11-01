use crate::{
    app::components::{
        playlist::explorer::{
            identity::Id,
            sort::{epg_badge_label_key, extract_channel_no, is_edited},
        },
        table::{PagedTable, TableDefinition},
        AppIcon,
    },
    i18n::use_translation,
};
use shared::model::UiPlaylistItem;
use std::rc::Rc;
use wasm_bindgen::JsCast;
use yew::prelude::*;

crate::app::components::define_table_columns! {
    enum LiveTableColumn {
        Actions => ("actions", "") { can_hide: false },
        Select => ("select", "") { can_hide: false },
        ChNo => ("chno", "LABEL.EXPLORER_SORT_CHANNEL_NO"),
        Logo => ("logo", "LABEL.LOGO"),
        Name => ("name", "LABEL.NAME") { can_hide: false },
        Epg => ("epg", "LABEL.EPG"),
        Badges => ("badges", "LABEL.STATUS"),
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
}

#[component]
pub fn ExplorerLiveView(props: &ExplorerLiveViewProps) -> Html {
    let translate = use_translation();
    let page = use_state(|| 1u32);
    let page_size = use_state(|| 50u16);

    let columns = use_memo((), |()| LiveTableColumn::columns());

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
                        <input
                            type="checkbox"
                            class="tp__checkbox"
                            aria-label={select_label}
                            checked={all_selected}
                            {onchange}
                        />
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
                        let on_popup_click = Callback::from(move |e: MouseEvent| {
                            e.prevent_default();
                            e.stop_propagation();
                            popup_cb.emit((pli_for_popup.clone(), e));
                        });
                        html! {
                            <button
                                type="button"
                                class="tp__icon-button"
                                aria-label={translate.t("LABEL.ACTIONS")}
                                onclick={on_popup_click}
                            >
                                <AppIcon name="Popup" />
                            </button>
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
                                <input
                                    type="checkbox"
                                    class="tp__checkbox"
                                    aria-label={select_label}
                                    checked={is_checked}
                                    {onchange}
                                />
                            </span>
                        }
                    }
                    Some(LiveTableColumn::ChNo) => {
                        let chno = extract_channel_no(&pli.name, &pli.title)
                            .map(|n| n.to_string())
                            .unwrap_or_else(|| "-".to_string());
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
                                html! { <span class="noepg">{ "⚠ " }{ translate.t(label_key) }</span> }
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
            table_id: "playlist.explorer.live".into(),
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
            />
        </div>
    }
}
