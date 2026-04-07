use crate::{
    app::components::{
        api_user::target_playlist::{BouquetSelection, UserTargetPlaylist},
        Panel, RadioButtonGroup, TextButton,
    },
    hooks::use_service_context,
    i18n::use_translation,
    model::{BusyStatus, EventMessage},
};
use shared::{
    error::TuliproxError,
    model::{PlaylistBouquetDto, PlaylistCategoriesDto, PlaylistClusterBouquetDto, PlaylistClusterCategoriesDto},
    utils::Internable,
};
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    fmt,
    rc::Rc,
    str::FromStr,
    sync::Arc,
};
use yew::prelude::*;

const XTREAM: &str = "xtream";
const M3U: &str = "m3u";

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum ApiUserPlaylistPage {
    Xtream,
    M3u,
}

impl ApiUserPlaylistPage {
    pub fn as_str(&self) -> &'static str {
        match self {
            ApiUserPlaylistPage::Xtream => XTREAM,
            ApiUserPlaylistPage::M3u => M3U,
        }
    }
}

impl FromStr for ApiUserPlaylistPage {
    type Err = TuliproxError;

    fn from_str(s: &str) -> Result<Self, TuliproxError> {
        match s.to_lowercase().as_str() {
            XTREAM => Ok(ApiUserPlaylistPage::Xtream),
            M3U => Ok(ApiUserPlaylistPage::M3u),
            _ => Err(TuliproxError::Config(format!("Unknown api user playlist type: {s}"))),
        }
    }
}

impl fmt::Display for ApiUserPlaylistPage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = self.as_str();
        write!(f, "{s}")
    }
}

impl Internable for ApiUserPlaylistPage {
    fn intern(self) -> Arc<str> { self.as_str().intern() }
}

fn selected_categories_or_none(map: &HashMap<String, bool>, available: Option<&Vec<String>>) -> Option<Vec<String>> {
    let available_set: HashSet<String> =
        available.map(|categories| categories.iter().cloned().collect()).unwrap_or_default();
    let mut filtered: Vec<String> = map
        .iter()
        .filter(|(category, is_selected)| **is_selected && (available.is_none() || available_set.contains(*category)))
        .map(|(category, _)| category.clone())
        .collect();

    filtered.sort();

    if filtered.is_empty() || (!available_set.is_empty() && filtered.len() == available_set.len()) {
        None
    } else {
        Some(filtered)
    }
}

fn to_playlist_cluster(
    categories: Option<&PlaylistClusterCategoriesDto>,
    bouquet: Option<&Rc<RefCell<BouquetSelection>>>,
) -> Option<PlaylistClusterBouquetDto> {
    if let Some(bouq) = bouquet {
        let selections = bouq.borrow();
        let live = selected_categories_or_none(&selections.live, categories.and_then(|value| value.live.as_ref()));
        let vod = selected_categories_or_none(&selections.vod, categories.and_then(|value| value.vod.as_ref()));
        let series =
            selected_categories_or_none(&selections.series, categories.and_then(|value| value.series.as_ref()));

        // if all three are None, return None
        if live.is_none() && vod.is_none() && series.is_none() {
            None
        } else {
            Some(PlaylistClusterBouquetDto { live, vod, series })
        }
    } else {
        None
    }
}

#[component]
pub fn ApiUserPlaylist() -> Html {
    let translate = use_translation();
    let service_ctx = use_service_context();
    let categories = use_state(|| None as Option<Rc<PlaylistCategoriesDto>>);
    let bouquets = use_state(|| None as Option<Rc<PlaylistBouquetDto>>);
    let active_tab = use_state(|| ApiUserPlaylistPage::Xtream);
    let playlist_types = use_memo((), |_| {
        [ApiUserPlaylistPage::Xtream, ApiUserPlaylistPage::M3u].iter().map(ToString::to_string).collect::<Vec<String>>()
    });

    // Selection reference
    let selections = use_mut_ref(HashMap::<ApiUserPlaylistPage, Rc<RefCell<BouquetSelection>>>::new);

    let handle_tab_select = {
        let active_tab_clone = active_tab.clone();
        Callback::from(move |page_selection: Rc<Vec<String>>| {
            if let Some(page_selection_str) = page_selection.first() {
                if let Ok(page) = ApiUserPlaylistPage::from_str(page_selection_str) {
                    active_tab_clone.set(page)
                }
            }
        })
    };

    {
        // ----- Load data on mount -----
        let categories = categories.clone();
        let bouquets = bouquets.clone();
        let services = service_ctx.clone();
        let translate = translate.clone();

        use_effect_with((), move |_| {
            wasm_bindgen_futures::spawn_local(async move {
                services.event.broadcast(EventMessage::Busy(BusyStatus::Show));
                let result =
                    (services.user_api.get_playlist_bouquet().await, services.user_api.get_playlist_categories().await);
                match result {
                    (Ok(bouquet), Ok(cats)) => {
                        bouquets.set(bouquet.clone());
                        categories.set(cats.clone());
                    }
                    (Err(e1), Err(e2)) => {
                        log::error!("Failed to load bouquet: {e1:?}, categories: {e2:?}");
                        services.toastr.error(translate.t("MESSAGES.DOWNLOAD.USER_BOUQUET.FAIL"));
                    }
                    (Err(e), _) | (_, Err(e)) => {
                        log::error!("Failed to load user data: {e:?}");
                        services.toastr.error(translate.t("MESSAGES.DOWNLOAD.USER_BOUQUET.FAIL"));
                    }
                }

                services.event.broadcast(EventMessage::Busy(BusyStatus::Hide));
            });
            || {}
        });
    }

    // ----- Save handler -----
    let on_save = {
        let selections = selections.clone();
        let services = service_ctx.clone();
        let translate = translate.clone();
        let categories = categories.clone();

        Callback::from(move |_| {
            let selections = selections.clone();
            let services = services.clone();
            let translate = translate.clone();
            let xtream_categories = categories.as_ref().and_then(|plc| plc.xtream.as_ref().cloned());
            let m3u_categories = categories.as_ref().and_then(|plc| plc.m3u.as_ref().cloned());

            wasm_bindgen_futures::spawn_local(async move {
                services.event.broadcast(EventMessage::Busy(BusyStatus::Show));
                let result = {
                    let selects = selections.borrow();
                    PlaylistBouquetDto {
                        xtream: to_playlist_cluster(
                            xtream_categories.as_ref(),
                            selects.get(&ApiUserPlaylistPage::Xtream),
                        ),
                        m3u: to_playlist_cluster(m3u_categories.as_ref(), selects.get(&ApiUserPlaylistPage::M3u)),
                    }
                };

                match services.user_api.save_playlist_bouquet(&result).await {
                    Ok(()) => services.toastr.success(translate.t("MESSAGES.SAVE.BOUQUET.SUCCESS")),
                    Err(_) => services.toastr.error(translate.t("MESSAGES.SAVE.BOUQUET.FAIL")),
                }

                services.event.broadcast(EventMessage::Busy(BusyStatus::Hide));
            });
        })
    };

    let handle_m3u_change = {
        let selections = selections.clone();
        Callback::from(move |selection: Rc<RefCell<BouquetSelection>>| {
            selections.borrow_mut().insert(ApiUserPlaylistPage::M3u, selection);
        })
    };

    let handle_xtream_change = {
        let selections = selections.clone();
        Callback::from(move |selection: Rc<RefCell<BouquetSelection>>| {
            selections.borrow_mut().insert(ApiUserPlaylistPage::Xtream, selection);
        })
    };

    let active_page = active_tab.intern();
    html! {
        <div class="tp__api-user-playlist">
            <div class="tp__api-user-playlist__header tp__list-list__header">
                <h1>{translate.t("TITLE.USER_BOUQUET_EDITOR") }</h1>
                <div class="tp__userlist-list__header-toolbar">
                    <TextButton class="secondary" name="save"
                            icon="Save"
                            title={ translate.t("LABEL.SAVE")}
                            onclick={on_save}></TextButton>

                </div>
            </div>

            <div class="tp__api-user-playlist__content">
                <div class="tp__api-user-playlist__content-toolbar">
                 <RadioButtonGroup options={playlist_types.clone()}
                                          selected={Rc::new(vec![(*active_tab).to_string()])}
                                          on_select={handle_tab_select} />
                </div>

                <div class="tp__api-user-playlist__content-panels">
                    <Panel value={ApiUserPlaylistPage::Xtream.intern()} active={active_page.clone()}>
                        <UserTargetPlaylist
                            categories={categories.as_ref().and_then(|c| c.xtream.clone())}
                            bouquet={bouquets.as_ref().and_then(|b| b.xtream.as_ref().cloned())}
                            on_change={handle_xtream_change.clone()}
                            ></UserTargetPlaylist>
                    </Panel>
                    <Panel value={ApiUserPlaylistPage::M3u.intern()} active={active_page}>
                       <UserTargetPlaylist
                            categories={categories.as_ref().and_then(|c| c.m3u.clone())}
                            bouquet={bouquets.as_ref().and_then(|b| b.m3u.as_ref().cloned())}
                            on_change={handle_m3u_change.clone()}
                            ></UserTargetPlaylist>
                    </Panel>
                </div>
            </div>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selected_categories_or_none_returns_none_when_all_unique_categories_are_selected() {
        let categories = vec!["Sport".to_string(), "News".to_string(), "Sport".to_string()];
        let selections = HashMap::from([("Sport".to_string(), true), ("News".to_string(), true)]);

        assert_eq!(selected_categories_or_none(&selections, Some(&categories)), None);
    }

    #[test]
    fn selected_categories_or_none_returns_selected_values_when_not_all_categories_are_selected() {
        let categories = vec!["Sport".to_string(), "News".to_string(), "Movies".to_string()];
        let selections =
            HashMap::from([("Sport".to_string(), true), ("News".to_string(), false), ("Movies".to_string(), true)]);

        assert_eq!(
            selected_categories_or_none(&selections, Some(&categories)),
            Some(vec!["Movies".to_string(), "Sport".to_string()])
        );
    }

    #[test]
    fn selected_categories_or_none_returns_none_for_empty_selection_map() {
        let categories = vec!["Sport".to_string(), "News".to_string()];
        let selections = HashMap::new();

        assert_eq!(selected_categories_or_none(&selections, Some(&categories)), None);
    }

    #[test]
    fn selected_categories_or_none_returns_none_when_all_selections_are_false() {
        let categories = vec!["Sport".to_string(), "News".to_string()];
        let selections = HashMap::from([("Sport".to_string(), false), ("News".to_string(), false)]);

        assert_eq!(selected_categories_or_none(&selections, Some(&categories)), None);
    }

    #[test]
    fn selected_categories_or_none_returns_sorted_selected_values_when_available_is_none() {
        let selections = HashMap::from([("Sport".to_string(), true), ("Movies".to_string(), true)]);

        assert_eq!(
            selected_categories_or_none(&selections, None),
            Some(vec!["Movies".to_string(), "Sport".to_string()])
        );
    }

    #[test]
    fn selected_categories_or_none_ignores_stale_selected_categories_not_in_available_list() {
        let categories = vec!["Sport".to_string(), "News".to_string()];
        let selections =
            HashMap::from([("Sport".to_string(), true), ("News".to_string(), true), ("Old".to_string(), true)]);

        assert_eq!(selected_categories_or_none(&selections, Some(&categories)), None);
    }
}
