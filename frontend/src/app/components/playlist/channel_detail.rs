//! Channel rendering of the playlist explorer: the channel tiles of a group and
//! the detail view of one item (series info with its episodes).
//!
//! These were closures inside `PlaylistExplorer`. The detail view is a component
//! now because it hosts the `channel-detail-panel` plugin slot (plan §11 slot 7).

use crate::{
    app::components::{AppIcon, Chip, PluginSlot},
    html_if,
    i18n::{use_translation, YewI18n},
};
use serde_json::json;
use shared::{
    model::{
        PluginChannelContextDto, PluginSlotDto, SeriesStreamDetailEpisodeProperties, SeriesStreamProperties,
        UiPlaylistItem, VirtualId, XtreamCluster,
    },
    utils::format_float_localized,
};
use std::{collections::HashMap, rc::Rc};
use wasm_bindgen::JsCast;
use yew::prelude::*;

/// What the context menu of the explorer acts on.
#[derive(Clone)]
pub(super) struct ChannelSelection {
    pub virtual_id: VirtualId,
    pub cluster: XtreamCluster,
    pub downloadable: bool,
    pub url: String,
    pub title: String,
    pub input_name: String,
    /// What a plugin may see of the channel; never carries a URL.
    pub plugin_context: PluginChannelContextDto,
}

/// The slot context of a playlist item. Stream and catch-up URLs embed provider
/// credentials, so none of them is copied.
pub(super) fn plugin_channel_context(item: &UiPlaylistItem) -> PluginChannelContextDto {
    PluginChannelContextDto {
        virtual_id: item.virtual_id,
        name: item.name.to_string(),
        title: item.title.to_string(),
        group: item.group.to_string(),
        item_type: item.item_type.as_str().to_string(),
        epg_channel_id: item.epg_channel_id.as_ref().map(ToString::to_string),
        input_name: (!item.input_name.is_empty()).then(|| item.input_name.to_string()),
    }
}

pub(super) fn render_channel_logo(translate: &YewI18n, logo: &str, title: &str) -> Html {
    let logo = if logo.is_empty() { "assets/missing-logo.svg".to_string() } else { logo.to_string() };
    let alt = if title.is_empty() { translate.t("LABEL.CHANNEL") } else { title.to_string() };
    html! {
        <span  class="tp__playlist-explorer__channel-logo">
            <img  alt={alt} src={logo} loading="lazy"
            onerror={Callback::from(move |e: web_sys::Event| {
            if let Some(target)  = e.target() {
                if let Ok(img) = target.dyn_into::<web_sys::HtmlImageElement>() {
                    img.set_src("assets/missing-logo.svg");
                }
            }
            })}/>
        </span>
    }
}

fn render_live(
    translate: &YewI18n,
    chan: &Rc<UiPlaylistItem>,
    handle_popup_onclick: &Callback<(Rc<UiPlaylistItem>, MouseEvent)>,
) -> Html {
    let popup_onclick = handle_popup_onclick.clone();
    let chan_clone = Rc::clone(chan);
    html! {
    <span class="tp__playlist-explorer__channel tp__playlist-explorer__channel-live">
        <button class="tp__icon-button" onclick={Callback::from(move |event: MouseEvent| popup_onclick.emit((chan_clone.clone(), event)))}>
            <AppIcon name="Popup"></AppIcon>
        </button>
        {render_channel_logo(translate, &chan.logo, &chan.title)}
        <span class="tp__playlist-explorer__channel-title">{chan.title.clone()}</span>
        </span>
    }
}

fn render_movie(
    translate: &YewI18n,
    chan: &Rc<UiPlaylistItem>,
    handle_popup_onclick: &Callback<(Rc<UiPlaylistItem>, MouseEvent)>,
) -> Html {
    let popup_onclick = handle_popup_onclick.clone();
    let chan_clone = Rc::clone(chan);
    html! {
        <span class="tp__playlist-explorer__channel tp__playlist-explorer__channel-video">
            {render_channel_logo(translate, &chan.logo, &chan.title)}
            {
                html_if!(chan.rating > 0.001, {
                    <Chip class="tp__playlist-explorer__channel-video-rating" label={format_float_localized(chan.rating, 1, false)} />
                })
            }
            <span class="tp__playlist-explorer__channel-video-info">
                <button class="tp__icon-button" onclick={Callback::from(move |event: MouseEvent| popup_onclick.emit((chan_clone.clone(), event)))}>
                    <AppIcon name="Popup"></AppIcon>
                </button>
                <span class="tp__playlist-explorer__channel-video-title">{chan.title.clone()}</span>
            </span>
        </span>
    }
}

fn render_series(
    translate: &YewI18n,
    chan: &Rc<UiPlaylistItem>,
    handle_popup_onclick: &Callback<(Rc<UiPlaylistItem>, MouseEvent)>,
    handle_series_onclick: &Callback<(Rc<UiPlaylistItem>, MouseEvent)>,
) -> Html {
    let popup_onclick = handle_popup_onclick.clone();
    let chan_clone = Rc::clone(chan);
    let chan_click = {
        let chan_clone = chan.clone();
        let series_click = handle_series_onclick.clone();
        Callback::from(move |event: MouseEvent| series_click.emit((chan_clone.clone(), event)))
    };
    html! {
        <span onclick={chan_click} class="tp__playlist-explorer__channel tp__playlist-explorer__channel-series">
            {render_channel_logo(translate, &chan.logo, &chan.title)}
            {
                html_if!(chan.rating > 0.001, {
                    <Chip class="tp__playlist-explorer__channel-series-rating" label={format_float_localized(chan.rating, 1, false)} />
                })
            }
            <span class="tp__playlist-explorer__channel-series-info">
                <button class="tp__icon-button" onclick={Callback::from(move |event: MouseEvent| popup_onclick.emit((chan_clone.clone(), event)))}>
                    <AppIcon name="Popup"></AppIcon>
                </button>
                <span class="tp__playlist-explorer__channel-series-title">{chan.title.clone()}</span>
            </span>
        </span>
    }
}

/// The tile of one channel in a group list.
pub(super) fn render_channel(
    translate: &YewI18n,
    chan: &Rc<UiPlaylistItem>,
    handle_popup_onclick: &Callback<(Rc<UiPlaylistItem>, MouseEvent)>,
    handle_series_onclick: &Callback<(Rc<UiPlaylistItem>, MouseEvent)>,
) -> Html {
    match chan.xtream_cluster {
        XtreamCluster::Live => render_live(translate, chan, handle_popup_onclick),
        XtreamCluster::Video => render_movie(translate, chan, handle_popup_onclick),
        XtreamCluster::Series => render_series(translate, chan, handle_popup_onclick, handle_series_onclick),
    }
}

fn render_episode(
    translate: &YewI18n,
    chan: &SeriesStreamDetailEpisodeProperties,
    series: &UiPlaylistItem,
    handle_episode_popup_onclick: &Callback<(ChannelSelection, MouseEvent)>,
) -> Html {
    let channel_select = ChannelSelection {
        virtual_id: VirtualId::new(chan.id),
        cluster: XtreamCluster::Series,
        downloadable: true,
        // Falls back to the episode fetch path in the menu handler when empty
        url: chan.direct_source.to_string(),
        title: chan.title.to_string(),
        input_name: String::new(),
        plugin_context: PluginChannelContextDto {
            virtual_id: chan.id,
            name: chan.title.to_string(),
            title: chan.title.to_string(),
            group: series.group.to_string(),
            item_type: shared::model::PlaylistItemType::Series.as_str().to_string(),
            epg_channel_id: None,
            input_name: None,
        },
    };
    let popup_onclick = handle_episode_popup_onclick.clone();
    let rating = chan.rating.unwrap_or_default();
    html! {
        <span class="tp__playlist-explorer__channel tp__playlist-explorer__channel-episode">
            {render_channel_logo(translate, &chan.movie_image, &chan.title)}
            {
                html_if!(rating > 0.001, {
                    <Chip class="tp__playlist-explorer__channel-episode-rating" label={format_float_localized(rating, 1, false)} />
                })
            }
            <span class="tp__playlist-explorer__channel-episode-info">
                <button class="tp__icon-button" onclick={Callback::from(move |event: MouseEvent| popup_onclick.emit((channel_select.clone(), event)))}>
                    <AppIcon name="Popup"></AppIcon>
                </button>
                <span class="tp__playlist-explorer__channel-episode-title">{chan.title.clone()}</span>
            </span>
        </span>
    }
}

#[derive(Properties, Clone)]
pub(super) struct ChannelDetailProps {
    /// The series item the detail belongs to.
    pub item: Rc<UiPlaylistItem>,
    /// `None` while the server could not deliver the series info.
    pub series: Option<Rc<SeriesStreamProperties>>,
    pub on_episode_popup: Callback<(ChannelSelection, MouseEvent)>,
}

impl PartialEq for ChannelDetailProps {
    fn eq(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.item, &other.item)
            && match (&self.series, &other.series) {
                (Some(left), Some(right)) => Rc::ptr_eq(left, right),
                (None, None) => true,
                _ => false,
            }
            && self.on_episode_popup == other.on_episode_popup
    }
}

/// The detail view of one item: header with the series info, its episodes by
/// season, and the plugin panels of the `channel-detail-panel` slot.
#[component]
pub(super) fn ChannelDetail(props: &ChannelDetailProps) -> Html {
    let translate = use_translation();
    let series_info = &props.item;
    // UiPlaylistItem has no additional_properties - props are passed in or None
    let (mut backdrop, plot, cast, genre, release_date, rating, details) = match props.series.as_deref() {
        Some(series_props) => {
            let backdrop = series_props.backdrop_path.as_ref().and_then(|l| l.first()).map_or_else(
                || {
                    if series_props.cover.is_empty() {
                        series_info.logo.to_string()
                    } else {
                        series_props.cover.to_string()
                    }
                },
                ToString::to_string,
            );
            (
                Some(backdrop.clone()),
                series_props.plot.as_deref().map(ToString::to_string).unwrap_or_default(),
                series_props.cast.to_string(),
                series_props.genre.as_deref().map(ToString::to_string).unwrap_or_default(),
                series_props.release_date.as_deref().map(ToString::to_string).unwrap_or_default(),
                series_props.rating,
                series_props.details.as_ref(),
            )
        }
        _ => (None, String::new(), String::new(), String::new(), String::new(), 0.0, None),
    };

    if !series_info.logo.is_empty() && backdrop.as_ref().is_none_or(std::string::String::is_empty) {
        backdrop = Some(series_info.logo.to_string());
    }

    let style = backdrop.as_ref().map(|b| format!("background-image: url(\"{b}\");")).unwrap_or_default();

    let series_html = html! {
        <div class="tp__playlist-explorer__series-info__body-top" style={style}>
            <div class="tp__playlist-explorer__series-info__body-top-backdrop"></div>
            <div class="tp__playlist-explorer__series-info__body-top-content">
                <span class="tp__playlist-explorer__series-info__title">{series_info.title.clone()}</span>
                <span class="tp__playlist-explorer__series-info__infos">
                    {
                        html_if!(rating > 0.001, {
                        <>
                         <span class="tp__playlist-explorer__series-info__nowrap">
                             <Chip class="tp__playlist-explorer__series-info__rating" label={format_float_localized(rating, 1, false)} />
                        </span>
                        {"◦"}
                        </>
                    })}
                    <span class="tp__playlist-explorer__series-info__nowrap">{release_date}</span>
                    {"◦"}
                    <span>{genre}</span>
                </span>
                <span class="tp__playlist-explorer__series-info__plot">{plot}</span>
                <span class="tp__playlist-explorer__series-info__cast">{cast}</span>
            </div>
        </div>
    };

    let episodes_html = if let Some(episodes) = details.as_ref().and_then(|d| d.episodes.as_ref()) {
        let mut grouped: HashMap<u32, Vec<&SeriesStreamDetailEpisodeProperties>> = HashMap::new();
        for item in episodes {
            grouped.entry(item.season).or_default().push(item);
        }
        let mut grouped_list: Vec<(u32, Vec<&SeriesStreamDetailEpisodeProperties>)> = grouped.into_iter().collect();
        grouped_list.sort_by_key(|(season, _)| *season);

        html! {
            for (season, season_episodes) in grouped_list.iter() {
                <div key={format!("season-{season}")}>
                    <div class={"tp__playlist-explorer__series-info__season"}>
                        <span class={"tp__playlist-explorer__series-info__season-title"}>{translate.t("LABEL.SEASON")} {" - "} {season}</span>
                    </div>
                    <div class={"tp__playlist-explorer__group-list tp__playlist-explorer__group-list-episodes"}>
                        for episode in season_episodes.iter() {
                            { render_episode(&translate, episode, series_info, &props.on_episode_popup) }
                        }
                    </div>
                </div>
            }
        }
    } else {
        Html::default()
    };

    let slot_context = Some(json!({ "channel": plugin_channel_context(series_info) }));

    html! {
    <div class="tp__playlist-explorer__series-info">
        <div class="tp__playlist-explorer__series-info__header">
            { series_html }
        </div>
         <div class="tp__playlist-explorer__series-info__body">
             {episodes_html}
        </div>
        <PluginSlot slot={PluginSlotDto::ChannelDetailPanel} context={slot_context} />
    </div>
    }
}
