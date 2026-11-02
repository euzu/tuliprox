use crate::{
    app::components::{
        playlist::explorer::{
            identity::Id,
            sort::{extract_added, extract_last_modified, extract_year, is_edited, is_within_new_window},
        },
        table::{PagedTable, TableDefinition, PAGE_SIZES},
        AppIcon, Checkbox, IconButton,
    },
    i18n::use_translation,
};
use shared::{
    model::{UiPlaylistItem, XtreamCluster},
    utils::format_float_localized,
};
use std::rc::Rc;
use wasm_bindgen::JsCast;
use yew::prelude::*;

const POSTER_GRID_PAGE_SIZE: usize = 120;

crate::app::components::define_table_columns! {
    enum VodTableColumn {
        Actions => ("actions", "") { can_hide: false, content: false },
        Select => ("select", "") { can_hide: false, content: false },
        Logo => ("logo", "LABEL.LOGO"),
        Title => ("title", "LABEL.TITLE") { can_hide: false },
        Year => ("year", "LABEL.EXPLORER_SORT_YEAR"),
        Rating => ("rating", "LABEL.EXPLORER_SORT_RATING"),
    }
}

#[derive(Properties, PartialEq, Clone)]
pub struct ExplorerPosterGridProps {
    pub cluster: XtreamCluster,
    /// `Id` compares by pointer, so the prop check never walks the filtered catalog.
    pub items: Id<Vec<Rc<UiPlaylistItem>>>,
    pub selected_item: Option<Id<UiPlaylistItem>>,
    pub on_select_item: Callback<Rc<UiPlaylistItem>>,
    pub on_popup_menu: Callback<(Rc<UiPlaylistItem>, MouseEvent)>,
    #[prop_or_default]
    pub on_open_series: Option<Callback<Rc<UiPlaylistItem>>>,
    #[prop_or_default]
    pub is_list_view: bool,
    #[prop_or_default]
    pub selected_ids: Rc<std::collections::HashSet<u32>>,
    #[prop_or_default]
    pub on_toggle_select: Callback<u32>,
    #[prop_or_default]
    pub on_select_page: Callback<(Vec<u32>, bool)>,
}

#[must_use]
fn current_unix_secs() -> u64 { (js_sys::Date::now() / 1000.0).max(0.0) as u64 }

/// "New" for VOD and "updated" for series, driven by real provider timestamps only.
/// The extractors yield `None` while the backend does not publish those fields, so no badge is
/// shown instead of a fabricated one.
#[must_use]
fn freshness_badge_key(item: &UiPlaylistItem, cluster: XtreamCluster, now: u64) -> Option<&'static str> {
    match cluster {
        XtreamCluster::Video if is_within_new_window(extract_added(item), now) => Some("LABEL.NEW"),
        XtreamCluster::Series if is_within_new_window(extract_last_modified(item), now) => {
            Some("LABEL.EXPLORER_UPDATED")
        }
        _ => None,
    }
}

#[component]
pub fn ExplorerPosterGrid(props: &ExplorerPosterGridProps) -> Html {
    let translate = use_translation();
    let page = use_state(|| 1u32);
    let list_page_size = use_state(|| PAGE_SIZES[1]);
    let columns = use_memo((), |()| VodTableColumn::columns());

    let is_vod_list = props.is_list_view && props.cluster == XtreamCluster::Video;
    let page_size_num = if is_vod_list { usize::from(*list_page_size).max(1) } else { POSTER_GRID_PAGE_SIZE };
    let total_items = props.items.len();
    let total_pages = total_items.div_ceil(page_size_num).max(1) as u32;
    let current_page = (*page).clamp(1, total_pages);

    let start_idx = ((current_page.saturating_sub(1) as usize) * page_size_num).min(total_items);
    let end_idx = (start_idx + page_size_num).min(total_items);
    let page_items = &props.items[start_idx..end_idx];

    let handle_prev = {
        let page = page.clone();
        Callback::from(move |_: MouseEvent| {
            page.set((*page).saturating_sub(1).max(1));
        })
    };

    let handle_next = {
        let page = page.clone();
        Callback::from(move |_: MouseEvent| {
            page.set((*page).saturating_add(1).min(total_pages));
        })
    };

    let handle_first = {
        let page = page.clone();
        Callback::from(move |_: MouseEvent| page.set(1))
    };

    let handle_last = {
        let page = page.clone();
        Callback::from(move |_: MouseEvent| page.set(total_pages))
    };

    if props.is_list_view && props.cluster == XtreamCluster::Video {
        let render_header_cell = {
            let translate = translate.clone();
            let items = props.items.clone();
            let selected_ids = props.selected_ids.clone();
            let on_select_page = props.on_select_page.clone();

            Callback::<usize, Html>::from(move |col: usize| match VodTableColumn::from_index(col) {
                // The row actions column carries no header text by project rule.
                Some(VodTableColumn::Actions) => html! {},
                Some(VodTableColumn::Select) => {
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

        let on_popup_menu = props.on_popup_menu.clone();
        let on_select_item = props.on_select_item.clone();
        let on_open_series = props.on_open_series.clone();
        let cluster = props.cluster;
        let translate_cell = translate.clone();
        let selected_ids = props.selected_ids.clone();
        let on_toggle_select = props.on_toggle_select.clone();

        let render_data_cell = Callback::<(usize, usize, Rc<UiPlaylistItem>), Html>::from(
            move |(_row, col, item): (usize, usize, Rc<UiPlaylistItem>)| {
                let pli = item.clone();
                match VodTableColumn::from_index(col) {
                    Some(VodTableColumn::Actions) => {
                        let pli_popup = pli.clone();
                        let popup_cb = on_popup_menu.clone();
                        html! {
                            <IconButton
                                name="row_actions"
                                icon="Popup"
                                aria_label={Some(translate_cell.t("LABEL.ACTIONS"))}
                                onclick={Callback::from(move |(_, e): (String, MouseEvent)| {
                                    popup_cb.emit((pli_popup.clone(), e));
                                })}
                            />
                        }
                    }
                    Some(VodTableColumn::Select) => {
                        let vid = pli.virtual_id;
                        let is_checked = selected_ids.contains(&vid);
                        let on_toggle_select = on_toggle_select.clone();
                        let onchange = Callback::from(move |e: Event| {
                            e.stop_propagation();
                            on_toggle_select.emit(vid);
                        });
                        let select_label = translate_cell.t("LABEL.EXPLORER_SELECTED_ITEMS");
                        html! {
                            <span class="row-cell-select" onclick={Callback::from(|e: MouseEvent| e.stop_propagation())}>
                                <Checkbox aria_label={Some(select_label)} checked={is_checked} onchange={onchange} />
                            </span>
                        }
                    }
                    Some(VodTableColumn::Logo) => {
                        let logo_src = if pli.logo.is_empty() {
                            "assets/missing-logo.svg".to_string()
                        } else {
                            pli.logo.to_string()
                        };
                        html! {
                            <span class="row-cell-logo">
                                <span class="logo row-cell-logo--poster">
                                    <img alt={pli.title.to_string()} src={logo_src} loading="lazy" />
                                </span>
                            </span>
                        }
                    }
                    Some(VodTableColumn::Title) => {
                        let on_select = on_select_item.clone();
                        let on_open = on_open_series.clone();
                        let pli_c = pli.clone();
                        let on_dblclick = {
                            let pli_c = pli_c.clone();
                            Callback::from(move |_: MouseEvent| {
                                if cluster == XtreamCluster::Series {
                                    if let Some(ref open_cb) = on_open {
                                        open_cb.emit(pli_c.clone());
                                    }
                                }
                            })
                        };
                        html! {
                            <button
                                type="button"
                                class="row-cell-nm"
                                onclick={Callback::from(move |_: MouseEvent| on_select.emit(pli_c.clone()))}
                                ondblclick={on_dblclick}
                            >
                                <b>{ pli.title.as_ref() }</b>
                                <small>{ pli.group.as_ref() }</small>
                            </button>
                        }
                    }
                    Some(VodTableColumn::Year) => {
                        let year = extract_year(&pli.name, &pli.title)
                            .map(|y| y.to_string())
                            .unwrap_or_else(|| "-".to_string());
                        html! { <span>{ year }</span> }
                    }
                    Some(VodTableColumn::Rating) => {
                        let rating = if pli.rating > 0.001 {
                            format!("★ {}", format_float_localized(pli.rating, 1, false))
                        } else {
                            "-".to_string()
                        };
                        html! { <span class="row-cell-rating">{ rating }</span> }
                    }
                    None => html! {},
                }
            },
        );

        let paged_items: Vec<Rc<UiPlaylistItem>> = page_items.to_vec();
        let table_definition = Rc::new(TableDefinition::<UiPlaylistItem> {
            table_id: "playlist.explorer.vod".into(),
            columns,
            row_key: Callback::from(|(_, item): (usize, Rc<UiPlaylistItem>)| item.virtual_id.to_string().into()),
            items: Some(Rc::new(paged_items)),
            on_sort: Callback::noop(),
            render_header_cell,
            render_data_cell,
        });

        let on_page_change = {
            let page = page.clone();
            Callback::from(move |p: u32| page.set(p))
        };

        let on_page_size_change = {
            let list_page_size = list_page_size.clone();
            let page = page.clone();
            Callback::from(move |new_size: u16| {
                if PAGE_SIZES.contains(&new_size) {
                    list_page_size.set(new_size);
                    page.set(1);
                }
            })
        };

        return html! {
            <div class="vod-table">
                <PagedTable<UiPlaylistItem>
                    definition={table_definition}
                    page={current_page}
                    page_size={*list_page_size}
                    total_items={total_items as u64}
                    total_pages={total_pages}
                    has_prev={current_page > 1}
                    has_next={current_page < total_pages}
                    on_page_change={on_page_change}
                    on_page_size_change={on_page_size_change}
                />
            </div>
        };
    }

    // One clock read per render instead of one per card.
    let now = current_unix_secs();
    let has_any_selection = !props.selected_ids.is_empty();

    html! {
        <div class="poster-grid-container">
            <div class={format!("grid {}", if has_any_selection { "has-selection" } else { "" })}>
                {
                    page_items.iter().map(|item| {
                        let pli = item.clone();
                        let has_logo = !pli.logo.is_empty();
                        let logo_src = pli.logo.to_string();
                        let year = extract_year(&pli.name, &pli.title);
                        // One title (wrapping) plus the year below; the group is shown by the sidebar/select.
                        let subtitle = year.map(|y| y.to_string()).unwrap_or_default();
                        let badge_key = freshness_badge_key(&pli, props.cluster, now);
                        let edited = is_edited(&pli);

                        let on_click = {
                            let on_select = props.on_select_item.clone();
                            let pli_clone = pli.clone();
                            Callback::from(move |_: MouseEvent| {
                                on_select.emit(pli_clone.clone());
                            })
                        };

                        let on_dblclick = {
                            let on_open = props.on_open_series.clone();
                            let pli_clone = pli.clone();
                            let cluster = props.cluster;
                            Callback::from(move |_: MouseEvent| {
                                if cluster == XtreamCluster::Series {
                                    if let Some(ref open_cb) = on_open {
                                        open_cb.emit(pli_clone.clone());
                                    }
                                }
                            })
                        };

                        let on_popup_click = {
                            let on_popup = props.on_popup_menu.clone();
                            let pli_clone = pli.clone();
                            Callback::from(move |(_, e): (String, MouseEvent)| {
                                on_popup.emit((pli_clone.clone(), e));
                            })
                        };

                        let vid = pli.virtual_id;
                        let is_checked = props.selected_ids.contains(&vid);
                        let on_toggle_select = props.on_toggle_select.clone();
                        let on_card_toggle = Callback::from(move |e: Event| {
                            e.stop_propagation();
                            on_toggle_select.emit(vid);
                        });

                        html! {
                            <div
                                key={format!("poster-{}", pli.virtual_id)}
                                class={format!("post {} {}",
                                    if props.selected_item.as_ref().is_some_and(|s| s.virtual_id == pli.virtual_id) { "selected" } else { "" },
                                    if is_checked { "multi-selected" } else { "" }
                                )}
                                onclick={on_click}
                                ondblclick={on_dblclick}
                            >
                                <div class="img-wrap">
                                    <span class="tp__poster-select" onclick={Callback::from(|e: MouseEvent| e.stop_propagation())}>
                                        <Checkbox aria_label={Some(translate.t("LABEL.EXPLORER_SELECTED_ITEMS"))} checked={is_checked} onchange={on_card_toggle} />
                                    </span>
                                    <img class="ph-icon" src="assets/missing-logo.svg" alt="" />
                                    {
                                        if has_logo {
                                            html! {
                                                <img
                                                    class="poster-img"
                                                    alt={pli.title.to_string()}
                                                    src={logo_src}
                                                    loading="lazy"
                                                    onerror={Callback::from(|e: web_sys::Event| {
                                                        if let Some(target) = e.target() {
                                                            if let Ok(el) = target.dyn_into::<web_sys::Element>() {
                                                                let _ = el.set_attribute("hidden", "true");
                                                                el.set_class_name("poster-img hidden");
                                                            }
                                                        }
                                                    })}
                                                />
                                            }
                                        } else {
                                            html! {}
                                        }
                                    }
                                    {
                                        if pli.rating > 0.001 {
                                            html! { <span class="rt">{ format!("★ {}", format_float_localized(pli.rating, 1, false)) }</span> }
                                        } else {
                                            html! {}
                                        }
                                    }
                                    {
                                        if let Some(key) = badge_key {
                                            html! { <span class="new">{ translate.t(key) }</span> }
                                        } else {
                                            html! {}
                                        }
                                    }
                                    <span class="post-markers">
                                        {
                                            if pli.override_state().is_some_and(|s| s.pending) {
                                                html! { <span class="pnd" title={translate.t("LABEL.EXPLORER_PENDING_UNTIL_UPDATE")}>{ "⏳" }</span> }
                                            } else {
                                                html! {}
                                            }
                                        }
                                        {
                                            if edited {
                                                html! { <span class="edd"></span> }
                                            } else {
                                                html! {}
                                            }
                                        }
                                    </span>
                                </div>
                                <div class="caption">
                                    <div class="caption-text">
                                        <b title={pli.title.to_string()}>{ pli.title.as_ref() }</b>
                                        <small>{ subtitle }</small>
                                    </div>
                                    <IconButton
                                        name="card_actions"
                                        icon="Popup"
                                        class="post-popup"
                                        aria_label={Some(translate.t("LABEL.ACTIONS"))}
                                        onclick={on_popup_click}
                                    />
                                </div>
                            </div>
                        }
                    }).collect::<Html>()
                }
            </div>

            {
                if total_pages > 1 {
                    html! {
                        <div class="tp__paged-table__controls tp__poster-pager">
                            <button
                                type="button"
                                class="tp__icon-button"
                                disabled={current_page <= 1}
                                onclick={handle_first}
                            >
                                <AppIcon name="ChevronDoubleLeft" />
                            </button>
                            <button
                                type="button"
                                class="tp__icon-button"
                                disabled={current_page <= 1}
                                onclick={handle_prev}
                            >
                                <AppIcon name="ChevronLeft" />
                            </button>
                            <span class="tp__paged-table__info">
                                { translate.t("LABEL.EXPLORER_PAGE_RANGE")
                                    .replace("{start}", &(start_idx + 1).to_string())
                                    .replace("{end}", &end_idx.to_string())
                                    .replace("{total}", &total_items.to_string()) }
                            </span>
                            <button
                                type="button"
                                class="tp__icon-button"
                                disabled={current_page >= total_pages}
                                onclick={handle_next}
                            >
                                <AppIcon name="ChevronRight" />
                            </button>
                            <button
                                type="button"
                                class="tp__icon-button"
                                disabled={current_page >= total_pages}
                                onclick={handle_last}
                            >
                                <AppIcon name="ChevronDoubleRight" />
                            </button>
                        </div>
                    }
                } else {
                    html! {}
                }
            }
        </div>
    }
}
