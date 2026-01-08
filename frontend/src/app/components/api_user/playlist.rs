use crate::app::components::api_user::target_playlist::{BouquetSelection, UserTargetPlaylist};
use crate::app::components::{Panel, RadioButtonGroup, TextButton};
use crate::hooks::use_service_context;
use crate::model::{BusyStatus, EventMessage};
use shared::error::{TuliproxError, info_err_res};
use shared::model::{PlaylistBouquetDto, PlaylistCategoriesDto, PlaylistClusterBouquetDto};
use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt;
use std::rc::Rc;
use std::str::FromStr;
use yew::prelude::*;
use yew_i18n::use_translation;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum ApiUserPlaylistPage {
    Xtream,
    M3u,
}

impl FromStr for ApiUserPlaylistPage {
    type Err = TuliproxError;

    fn from_str(s: &str) -> Result<Self, TuliproxError> {
        match s.to_lowercase().as_str() {
            "xtream" => Ok(ApiUserPlaylistPage::Xtream),
            "m3u" => Ok(ApiUserPlaylistPage::M3u),
            _ => info_err_res!("Unknown api user playlist type: {s}"),
        }
    }
}

impl fmt::Display for ApiUserPlaylistPage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            ApiUserPlaylistPage::Xtream => "xtream",
            ApiUserPlaylistPage::M3u => "m3u",
        };
        write!(f, "{s}")
    }
}

fn to_playlist_cluster(count: (usize, usize, usize), bouquet: Option<&Rc<RefCell<BouquetSelection>>>) -> Option<PlaylistClusterBouquetDto> {
    if let Some(bouq) = bouquet {
        let selections = bouq.borrow();

        let selected_vec = |map: &HashMap<String, bool>| {
            let v: Vec<String> = map.iter()
                .filter(|(_, &selected)| selected)
                .map(|(c, _)| c.clone())
                .collect();
            if v.is_empty() { None } else { Some(v) }
        };

        let live = selected_vec(&selections.live).filter(|v| v.len() != count.0);
        let vod = selected_vec(&selections.vod).filter(|v| v.len() != count.1);
        let series = selected_vec(&selections.series).filter(|v| v.len() != count.2);

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

#[function_component]
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
    let selections = use_mut_ref(|| {
        HashMap::<ApiUserPlaylistPage, Rc<RefCell<BouquetSelection>>>::new()
    });

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
                let result = (services.user_api.get_playlist_bouquet().await, services.user_api.get_playlist_categories().await);
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
            let categories_xtream_count = categories.as_ref().and_then(|plc| plc.xtream.as_ref().map(|x|
                (x.live.as_ref().map(|v| v.len()).unwrap_or(0),
                 x.vod.as_ref().map(|v| v.len()).unwrap_or(0),
                 x.series.as_ref().map(|v| v.len()).unwrap_or(0))
            )).unwrap_or((0, 0, 0));
            let categories_m3u_count = categories.as_ref().and_then(|plc| plc.m3u.as_ref().map(|x|
                (x.live.as_ref().map(|v| v.len()).unwrap_or(0),
                 x.vod.as_ref().map(|v| v.len()).unwrap_or(0),
                 x.series.as_ref().map(|v| v.len()).unwrap_or(0))
            )).unwrap_or((0, 0, 0));

            wasm_bindgen_futures::spawn_local(async move {
                services.event.broadcast(EventMessage::Busy(BusyStatus::Show));
                let result = {
                    let selects = selections.borrow();
                    PlaylistBouquetDto {
                        xtream: to_playlist_cluster(categories_xtream_count, selects.get(&ApiUserPlaylistPage::Xtream)),
                        m3u: to_playlist_cluster(categories_m3u_count, selects.get(&ApiUserPlaylistPage::M3u)),
                    }
                };

                match services.user_api.save_playlist_bouquet(&result).await {
                    Ok(()) =>  services.toastr.success(translate.t("MESSAGES.SAVE.BOUQUET.SUCCESS")),
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

    html! {
        <div class="tp__api-user-playlist">
            <div class="tp__api-user-playlist__header tp__list-list__header">
                <h1>{translate.t("TITLE.USER_BOUQUET_EDITOR") }</h1>
                <div class="tp__userlist-list__header-toolbar">
                    <TextButton class="primary" name="save"
                            icon="Save"
                            title={ translate.t("LABEL.SAVE")}
                            onclick={on_save}></TextButton>

                </div>
            </div>

            <div class="tp__api-user-playlist__content">
                <div class="user-playlist__content-toolbar">
                 <RadioButtonGroup options={playlist_types.clone()}
                                          selected={Rc::new(vec![(*active_tab).to_string()])}
                                          on_select={handle_tab_select} />
                </div>

                <div class="tp__api-user-playlist__content-panels">
                    <Panel value={ApiUserPlaylistPage::Xtream.to_string()} active={active_tab.to_string()}>
                        <UserTargetPlaylist
                            categories={categories.as_ref().and_then(|c| c.xtream.clone())}
                            bouquet={bouquets.as_ref().and_then(|b| b.xtream.as_ref().cloned())}
                            on_change={handle_xtream_change.clone()}
                            ></UserTargetPlaylist>
                    </Panel>
                    <Panel value={ApiUserPlaylistPage::M3u.to_string()} active={active_tab.to_string()}>
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
