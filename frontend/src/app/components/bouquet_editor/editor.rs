use crate::{
    app::components::{AppIcon, Card, CollapsePanel, IconButton, RadioButtonGroup, Search},
    html_if,
    i18n::use_translation,
};
use shared::{
    error::TuliproxError,
    model::{ClusterFlags, PlaylistClusterBouquetDto, PlaylistClusterCategoriesDto, SearchRequest, XtreamCluster},
};
use std::{
    cell::RefCell,
    collections::HashMap,
    fmt::{Display, Formatter},
    rc::Rc,
    str::FromStr,
};
use wasm_bindgen::JsCast;
use yew::prelude::*;

fn normalize(s: &str) -> String {
    let cleaned: String = s.chars().filter(|c| c.is_alphanumeric() || c.is_whitespace()).collect();
    cleaned.trim().to_lowercase()
}

fn sort_opt_vec(v: &mut Option<Vec<String>>) {
    if let Some(ref mut inner) = v {
        inner.sort_by_cached_key(|a| normalize(a));
    }
}

fn resolve_category_element(target: Option<web_sys::EventTarget>) -> Option<web_sys::Element> {
    let mut current = target.and_then(|target| target.dyn_into::<web_sys::Node>().ok());
    while let Some(node) = current {
        if let Ok(element) = node.clone().dyn_into::<web_sys::Element>() {
            if element.has_attribute("data-cluster") && element.has_attribute("data-category") {
                return Some(element);
            }
            current = element.parent_element().map(Into::into);
        } else {
            current = node.parent_node();
        }
    }
    None
}

macro_rules! create_selection {
    ($bouquet:expr, $categories:expr, $selections:expr, $field: ident) => {
        if let Some(selects) = $bouquet.$field.as_ref() {
            for b in selects {
                $selections.$field.insert(b.clone(), true);
            }
        } else if let Some(cats) = $categories.$field.as_ref() {
            for c in cats {
                $selections.$field.insert(c.clone(), true);
            }
        }
    };
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FilterState {
    All,
    Selected,
    Deselected,
}

impl FilterState {
    const ALL: &'static str = "All";
    const SELECTED: &'static str = "Selected";
    const DESELECTED: &'static str = "Deselected";
}

impl Display for FilterState {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}",
            match self {
                Self::All => Self::ALL,
                Self::Selected => Self::SELECTED,
                Self::Deselected => Self::DESELECTED,
            }
        )
    }
}

impl FromStr for FilterState {
    type Err = TuliproxError;

    fn from_str(s: &str) -> Result<Self, TuliproxError> {
        match s {
            Self::ALL => Ok(Self::All),
            Self::SELECTED => Ok(Self::Selected),
            Self::DESELECTED => Ok(Self::Deselected),
            _ => Err(TuliproxError::Config(format!("Unknown FilterState: {s}"))),
        }
    }
}

#[derive(Clone, PartialEq, Default, Debug)]
pub struct BouquetSelection {
    pub live: HashMap<String, bool>,
    pub vod: HashMap<String, bool>,
    pub series: HashMap<String, bool>,
}

pub type BouquetOrigins = HashMap<XtreamCluster, HashMap<String, Vec<String>>>;

impl BouquetSelection {
    pub fn to_target_dto(&self, categories: &PlaylistClusterCategoriesDto) -> PlaylistClusterBouquetDto {
        PlaylistClusterBouquetDto {
            live: selected_categories_or_none(&self.live, categories.live.as_deref()),
            vod: selected_categories_or_none(&self.vod, categories.vod.as_deref()),
            series: selected_categories_or_none(&self.series, categories.series.as_deref()),
        }
    }
}

fn selected_categories_or_none(map: &HashMap<String, bool>, available: Option<&[String]>) -> Option<Vec<String>> {
    let mut selected: Vec<String> =
        map.iter().filter(|(_, is_selected)| **is_selected).map(|(category, _)| category.clone()).collect();
    selected.sort_unstable();
    selected.dedup();

    if selected.is_empty() {
        return None;
    }

    let available_set: std::collections::HashSet<&str> =
        available.unwrap_or_default().iter().map(String::as_str).collect();
    if !available_set.is_empty()
        && selected.len() == available_set.len()
        && selected.iter().all(|category| available_set.contains(category.as_str()))
    {
        None
    } else {
        Some(selected)
    }
}

#[derive(Properties, PartialEq)]
pub struct BouquetEditorProps {
    pub categories: Option<PlaylistClusterCategoriesDto>,
    pub bouquet: Option<PlaylistClusterBouquetDto>,
    pub on_change: Callback<Rc<RefCell<BouquetSelection>>>,
    #[prop_or_default]
    pub cluster_flags: Option<ClusterFlags>,
    #[prop_or_default]
    pub origins: Option<Rc<BouquetOrigins>>,
}

#[component]
pub fn BouquetEditor(props: &BouquetEditorProps) -> Html {
    let translate = use_translation();
    let bouquet_selection = use_mut_ref(BouquetSelection::default);
    let playlist_categories = use_state(PlaylistClusterCategoriesDto::default);
    let filter_state = use_state(HashMap::<XtreamCluster, FilterState>::new);
    let collapse_state = use_state(HashMap::<XtreamCluster, bool>::new);
    let force_update = use_state(|| 0);
    let search_filter = use_state::<SearchRequest, _>(|| SearchRequest::Clear);
    // Anchor for shift+click range selection: the last single-clicked category per cluster.
    let anchor = use_state(HashMap::<XtreamCluster, String>::new);
    // Mousedown position of the last category press, used by the click handler to tell a
    // real click apart from a click+drag (text selection) so dragging over the text to
    // highlight it does not also toggle the category.
    let mousedown_info = use_state::<Option<(XtreamCluster, String, i32, i32)>, _>(|| None);

    let handle_search = {
        let search_filter = search_filter.clone();
        Callback::from(move |req: SearchRequest| search_filter.set(req))
    };

    let handle_category_mousedown = {
        let mousedown_info = mousedown_info.clone();
        Callback::from(move |e: MouseEvent| {
            if e.shift_key() {
                e.prevent_default();
            }
            if let Some(element) = resolve_category_element(e.target()) {
                if let Some(cluster_attr) = element.get_attribute("data-cluster") {
                    if let Ok(cluster_enum) = XtreamCluster::from_str(cluster_attr.as_str()) {
                        if let Some(category) = element.get_attribute("data-category") {
                            mousedown_info.set(Some((cluster_enum, category, e.client_x(), e.client_y())));
                        }
                    }
                }
            }
        })
    };

    {
        let bouquet_selection = bouquet_selection.clone();
        let playlist_categories = playlist_categories.clone();
        let in_cats = props.categories.clone();
        let in_bouquet = props.bouquet.clone();
        let force_update = force_update.clone();
        use_effect_with((in_cats, in_bouquet), move |(maybe_categories, maybe_bouquet)| {
            let mut selections = BouquetSelection::default();
            if let Some(categories) = maybe_categories.as_ref() {
                if let Some(bouquet) = maybe_bouquet.as_ref() {
                    create_selection!(bouquet, categories, selections, live);
                    create_selection!(bouquet, categories, selections, vod);
                    create_selection!(bouquet, categories, selections, series);
                } else {
                    if let Some(cats) = categories.live.as_ref() {
                        for c in cats {
                            selections.live.insert(c.clone(), true);
                        }
                    }
                    if let Some(cats) = categories.vod.as_ref() {
                        for c in cats {
                            selections.vod.insert(c.clone(), true);
                        }
                    }
                    if let Some(cats) = categories.series.as_ref() {
                        for c in cats {
                            selections.series.insert(c.clone(), true);
                        }
                    }
                }
                *bouquet_selection.borrow_mut() = selections;
                let mut new_categories = categories.clone();
                sort_opt_vec(&mut new_categories.live);
                sort_opt_vec(&mut new_categories.vod);
                sort_opt_vec(&mut new_categories.series);
                playlist_categories.set(new_categories);
                force_update.set(*force_update + 1);
            }
        });
    }

    let handle_category_click = {
        let on_change = props.on_change.clone();
        let bouquet_selection = bouquet_selection.clone();
        let force_update = force_update.clone();
        let anchor = anchor.clone();
        let playlist_categories = playlist_categories.clone();
        let mousedown_info = mousedown_info.clone();
        Callback::from(move |e: MouseEvent| {
            if let Some(element) = resolve_category_element(e.target()) {
                if let Some(cluster_attr) = element.get_attribute("data-cluster") {
                    if let Ok(cluster_enum) = XtreamCluster::from_str(cluster_attr.as_str()) {
                        if let Some(category) = element.get_attribute("data-category") {
                            let is_drag =
                                (*mousedown_info).as_ref().is_some_and(|(mousedown_cluster, mousedown_cat, x, y)| {
                                    *mousedown_cluster != cluster_enum
                                        || mousedown_cat != &category
                                        || (e.client_x() - x).abs() > 2
                                        || (e.client_y() - y).abs() > 2
                                });
                            mousedown_info.set(None);

                            if is_drag {
                                return;
                            }

                            e.prevent_default();
                            e.stop_propagation();

                            let shift = e.shift_key();
                            {
                                let mut selections = bouquet_selection.borrow_mut();
                                let map = match cluster_enum {
                                    XtreamCluster::Live => &mut selections.live,
                                    XtreamCluster::Video => &mut selections.vod,
                                    XtreamCluster::Series => &mut selections.series,
                                };
                                if shift {
                                    if let Some(anchor_cat) = anchor.get(&cluster_enum).cloned() {
                                        let all_cats: Option<&Vec<String>> = match cluster_enum {
                                            XtreamCluster::Live => playlist_categories.live.as_ref(),
                                            XtreamCluster::Video => playlist_categories.vod.as_ref(),
                                            XtreamCluster::Series => playlist_categories.series.as_ref(),
                                        };
                                        if let Some(cats) = all_cats {
                                            if let (Some(start), Some(end)) = (
                                                cats.iter().position(|c| c == &anchor_cat),
                                                cats.iter().position(|c| c == &category),
                                            ) {
                                                let (from, to) = if start < end { (start, end) } else { (end, start) };
                                                let target_state = !*map.get(&category).unwrap_or(&false);
                                                for cat in &cats[from..=to] {
                                                    map.insert(cat.clone(), target_state);
                                                }
                                            }
                                        }
                                    } else {
                                        let selected = *map.get(&category).unwrap_or(&false);
                                        map.insert(category.clone(), !selected);
                                    }
                                    if let Some(window) = web_sys::window() {
                                        if let Some(document) = window.document() {
                                            if let Ok(Some(selection)) = document.get_selection() {
                                                let _ = selection.remove_all_ranges();
                                            }
                                        }
                                    }
                                } else {
                                    let mut new_anchor = (*anchor).clone();
                                    new_anchor.insert(cluster_enum, category.clone());
                                    anchor.set(new_anchor);
                                    let selected = *map.get(&category).unwrap_or(&false);
                                    map.insert(category.clone(), !selected);
                                }
                            }
                            on_change.emit(bouquet_selection.clone());
                            force_update.set(*force_update + 1);
                        }
                    }
                }
            }
        })
    };

    let handle_selection_change = {
        let bouquet_selection = bouquet_selection.clone();
        let on_change = props.on_change.clone();
        let force_update = force_update.clone();

        Callback::from(move |(cluster, cats, select): (XtreamCluster, Vec<String>, bool)| {
            {
                let mut selections = bouquet_selection.borrow_mut();
                let map = match cluster {
                    XtreamCluster::Live => &mut selections.live,
                    XtreamCluster::Video => &mut selections.vod,
                    XtreamCluster::Series => &mut selections.series,
                };
                for cat in cats {
                    map.insert(cat, select);
                }
            }
            on_change.emit(bouquet_selection.clone());
            force_update.set(*force_update + 1);
        })
    };

    let render_cluster_stats = |selected_count: usize, visible_count: usize, total: usize| {
        html! {
          <span class="tp__api-user-target-playlist__cluster-stats">
               <span class="tp__api-user-target-playlist__cluster-stats--label">{translate.t("LABEL.SELECTED")}</span>
               <span class="tp__api-user-target-playlist__cluster-stats--selected">{selected_count}</span>
               <span class="tp__api-user-target-playlist__cluster-stats--label">{translate.t("LABEL.FILTERED")}</span>
               <span class="tp__api-user-target-playlist__cluster-stats--filtered">{visible_count}</span>
               <span class="tp__api-user-target-playlist__cluster-stats--label">{translate.t("LABEL.TOTAL")}</span>
               <span class="tp__api-user-target-playlist__cluster-stats--total">{total}</span>
          </span>
        }
    };

    let render_category_cluster = |cluster: XtreamCluster,
                                   cats: Option<&Vec<String>>,
                                   selections: &HashMap<String, bool>| {
        if let Some(c) = cats {
            let cluster_clone = cluster;
            let handler = handle_selection_change.clone();
            let current_filter = *filter_state.get(&cluster).unwrap_or(&FilterState::All);

            let is_visible = |cat: &str| {
                let selected = *selections.get(cat).unwrap_or(&false);
                let filter_ok = match current_filter {
                    FilterState::All => true,
                    FilterState::Selected => selected,
                    FilterState::Deselected => !selected,
                };
                let search_ok = match &*search_filter {
                    SearchRequest::Clear => true,
                    SearchRequest::Text(pattern, _) => {
                        let lc = pattern.to_lowercase();
                        cat.to_lowercase().contains(&lc)
                    }
                    SearchRequest::Regexp(pattern, _) => {
                        shared::model::REGEX_CACHE.get_or_compile(pattern).is_ok_and(|re| re.is_match(cat))
                    }
                };
                filter_ok && search_ok
            };

            let visible_cats: Vec<String> = c.iter().filter(|cat| is_visible(cat)).cloned().collect();

            let total = c.len();
            let selected_count = c.iter().filter(|cat| *selections.get(*cat).unwrap_or(&false)).count();
            let visible_count = visible_cats.len();

            let select_all = {
                let cats = visible_cats.clone();
                let handler = handler.clone();
                Callback::from(move |(_, event): (String, MouseEvent)| {
                    event.stop_propagation();
                    handler.emit((cluster_clone, cats.clone(), true));
                })
            };

            let deselect_all = {
                let cats = visible_cats;
                let handler = handler.clone();
                Callback::from(move |(_, event): (String, MouseEvent)| {
                    event.stop_propagation();
                    handler.emit((cluster_clone, cats.clone(), false));
                })
            };

            let filter_state_handle = filter_state.clone();
            let filter_state_selections =
                Rc::new(vec![filter_state.get(&cluster).copied().unwrap_or(FilterState::All).to_string()]);
            let title_content = if *collapse_state.get(&cluster).unwrap_or(&true) {
                html! {
                <div class="tp__api-user-target-playlist__section-header">
                    <div class="tp__api-user-target-playlist__section-header__title">
                        {translate.t( match cluster {
                                XtreamCluster::Live =>  "LABEL.LIVE",
                                XtreamCluster::Video =>  "LABEL.MOVIE",
                                XtreamCluster::Series =>  "LABEL.SERIES"
                            })}
                    </div>
                    { render_cluster_stats(selected_count, visible_count, total) }
                    <div class="tp__api-user-target-playlist__section-header__toolbar">
                        <RadioButtonGroup
                                multi_select={false} none_allowed={false}
                                on_select={Callback::from(move |selections: Rc<Vec<String>>| {
                                    if let Some(first) = selections.first() {
                                       let mut cluster_state = (*filter_state_handle).clone();
                                       cluster_state.insert(cluster, FilterState::from_str(first.as_str()).unwrap_or(FilterState::All));
                                       filter_state_handle.set(cluster_state);
                                    }
                                })}
                                options={Rc::new([FilterState::All, FilterState::Selected, FilterState::Deselected].iter().map(std::string::ToString::to_string).collect::<Vec<String>>())}
                                labels={Rc::new(vec![translate.t("LABEL.ALL"), translate.t("LABEL.SELECTED"), translate.t("LABEL.DESELECTED")])}
                                selected={filter_state_selections}
                        />
                        <IconButton hint={translate.t("LABEL.SELECT_ALL")} name="SelectAll" icon="SelectAll" onclick={select_all} />
                        <IconButton hint={translate.t("LABEL.DESELECT_ALL")} name="DeselectAll" icon="DeselectAll" onclick={deselect_all} />
                    </div>
                </div>
                }
            } else {
                html! {
                    <>
                    <div class="tp__api-user-target-playlist__section-header__title">
                        {translate.t( match cluster {
                                XtreamCluster::Live =>  "LABEL.LIVE",
                                XtreamCluster::Video =>  "LABEL.MOVIE",
                                XtreamCluster::Series =>  "LABEL.SERIES"
                            })}
                    </div>
                    { render_cluster_stats(selected_count, visible_count, total) }
                    </>
                }
            };

            let collapse_state = collapse_state.clone();
            html_if!(!c.is_empty(), {
               <Card>
                  <CollapsePanel title_content={title_content} on_state_change={Callback::from(move |expanded| {
                       let mut collapse_map = (*collapse_state).clone();
                       collapse_map.insert(cluster, expanded);
                       collapse_state.set(collapse_map);
                  })}>
                    <div class="tp__api-user-target-playlist__categories">
                        { for c.iter().filter(|cat| is_visible(cat)).map(|cat| {
                            let selected = *selections.get(cat).unwrap_or(&false);
                            let origins = props
                                .origins
                                .as_ref()
                                .and_then(|by_cluster| by_cluster.get(&cluster))
                                .and_then(|by_name| by_name.get(cat));
                            html! {
                            <div key={cat.clone()} data-cluster={cluster.to_string()} data-category={cat.clone()} class={classes!("tp__api-user-target-playlist__categories-category", if selected {"selected"} else {""})}
                                onmousedown={handle_category_mousedown.clone()}
                                onclick={handle_category_click.clone()}>
                                <AppIcon name={if selected {"Checked"} else {"Unchecked"}}/> { &cat }
                                { origins.map_or_else(
                                    || html! {},
                                    |values| html! {
                                        <span class="tp__target-bouquet-view__origins" title={values.join(", ")}>
                                            { values.join(", ") }
                                        </span>
                                    },
                                ) }
                            </div>
                        }})}
                    </div>
                 </CollapsePanel>
                </Card>
            })
        } else {
            html! {}
        }
    };

    let selections = &*bouquet_selection.borrow();
    let cluster_flags = props.cluster_flags.unwrap_or(ClusterFlags::all());

    html! {
        <div class={"tp__api-user-target-playlist"}>
            <div class="tp__api-user-target-playlist__toolbar">
                <Search onsearch={Some(handle_search)} min_length={1} />
            </div>
            <div class="tp__api-user-target-playlist__body">
                { if cluster_flags.has_cluster(XtreamCluster::Live.into()) {
                    render_category_cluster(XtreamCluster::Live, playlist_categories.live.as_ref(), &selections.live)
                } else { html! {} } }
                { if cluster_flags.has_cluster(XtreamCluster::Video.into()) {
                    render_category_cluster(XtreamCluster::Video, playlist_categories.vod.as_ref(), &selections.vod)
                } else { html! {} } }
                { if cluster_flags.has_cluster(XtreamCluster::Series.into()) {
                    render_category_cluster(XtreamCluster::Series, playlist_categories.series.as_ref(), &selections.series)
                } else { html! {} } }
            </div>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_dto_normalizes_all_selected_to_unrestricted() {
        let categories = PlaylistClusterCategoriesDto {
            live: Some(vec!["News".to_string(), "Sports".to_string()]),
            vod: None,
            series: None,
        };
        let selection = BouquetSelection {
            live: HashMap::from([("News".to_string(), true), ("Sports".to_string(), true)]),
            ..Default::default()
        };

        assert_eq!(selection.to_target_dto(&categories).live, None);
    }

    #[test]
    fn target_dto_preserves_selected_groups_missing_from_catalog() {
        let categories = PlaylistClusterCategoriesDto { live: Some(vec!["News".to_string()]), vod: None, series: None };
        let selection = BouquetSelection {
            live: HashMap::from([("News".to_string(), true), ("Temporarily Missing".to_string(), true)]),
            ..Default::default()
        };

        assert_eq!(
            selection.to_target_dto(&categories).live,
            Some(vec!["News".to_string(), "Temporarily Missing".to_string()])
        );
    }
}
