use crate::{
    app::{
        components::{
            recording::{
                ensure_recording_available, epg_programme_to_prefill, target_name_for_id, EpgProgrammePrefillInput,
                PaddingBounds, RecordingForm,
            },
            EpgSourceSelector, HorizontalShrinkPanel, NoContent, Search,
        },
        context::ConfigContext,
    },
    hooks::use_service_context,
    i18n::use_translation,
    model::{BusyStatus, DialogAction, DialogActions, DialogResult, EventMessage},
    services::{CreateRecordingTaskRequest, DialogService, RecordingService, RecordingSourceInput},
    utils::{is_mobile_viewport, read_css_var_px, set_timeout},
};
use chrono::{Datelike, Local, TimeZone, Utc};
use gloo_timers::callback::Interval;
use shared::{
    concat_string,
    model::{
        AppConfigDto, EpgChannelFilter, EpgGroupInfo, Permission, PlaylistEpgRequest, SearchRequest, XtreamCluster,
        MAX_EPG_GRID_ROWS,
    },
};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    sync::Arc,
};
use wasm_bindgen::{prelude::Closure, JsCast};
use web_sys::{window, HtmlElement, MouseEvent, TouchEvent, WheelEvent};
use yew::{
    component, html, platform::spawn_local, use_callback, use_context, use_effect, use_effect_with, use_memo,
    use_mut_ref, use_node_ref, use_state, use_state_eq, AttrValue, Callback, Html, NodeRef, UseStateSetter,
};

mod group_list;
mod layout;
mod model;
mod row;

use group_list::EpgGroupList;
use layout::{
    anchor_scroll_left, compute_panned_scroll, compute_viewport, min_to_px, minute_at, shift_anchor, wheel_zoom_factor,
    Geometry, ScrollState, TimelineZoom, Viewport, ZoomAnchor, DEFAULT_PIXELS_PER_MIN, FALLBACK_HEADER_HEIGHT_PX,
    FALLBACK_ROW_HEIGHT_PX, TIME_BLOCK_MINS, ZOOM_EQUALITY_TOLERANCE,
};
use model::{
    cached_offsets, filter_channel_indices, finish_load, local_offset_secs, EpgGridChannel, EpgGridModel, ModelRef,
    RequestSeq,
};
use row::EpgRow;

const PROGRAM_GRID_PANNING_CLASS: &str = "tp__epg__program-grid-panning";
const TIMELINE_PANNING_CLASS: &str = "tp__epg__timeline-shell-panning";

type MouseHandle = Rc<RefCell<Option<Closure<dyn FnMut(MouseEvent)>>>>;
type PanListeners = Rc<RefCell<Option<(MouseHandle, MouseHandle)>>>;
type RafClosure = Rc<RefCell<Option<Closure<dyn FnMut(f64)>>>>;

fn register_mouse_pan(
    on_move: Box<dyn FnMut(MouseEvent)>,
    on_up: Box<dyn FnMut(MouseEvent)>,
) -> Option<(MouseHandle, MouseHandle)> {
    let win = window()?;
    let mouse_move = Closure::wrap(on_move);
    let _ = win.add_event_listener_with_callback("mousemove", mouse_move.as_ref().unchecked_ref());
    let mouse_up = Closure::wrap(on_up);
    let _ = win.add_event_listener_with_callback("mouseup", mouse_up.as_ref().unchecked_ref());
    Some((Rc::new(RefCell::new(Some(mouse_move))), Rc::new(RefCell::new(Some(mouse_up)))))
}

/// A move without the primary button held means the release happened outside the
/// window, so the pan has to end here.
fn is_primary_button_held(e: &MouseEvent) -> bool { e.buttons() & 1 != 0 }

/// Ends a pan: removes the panning class and the window listeners. The listener closures may be
/// executing right now (this is called from `mouseup`/`mousemove`), so they are dropped later.
fn end_pan(listeners: &PanListeners, element: Option<HtmlElement>, panning_class: &str) {
    if let Some(element) = element {
        let _ = element.class_list().remove_1(panning_class);
    }
    let Some((move_handle, up_handle)) = listeners.borrow_mut().take() else { return };
    if let Some(win) = window() {
        if let Some(mouse_move) = move_handle.borrow().as_ref() {
            let _ = win.remove_event_listener_with_callback("mousemove", mouse_move.as_ref().unchecked_ref());
        }
        if let Some(mouse_up) = up_handle.borrow().as_ref() {
            let _ = win.remove_event_listener_with_callback("mouseup", mouse_up.as_ref().unchecked_ref());
        }
    }
    set_timeout(move || drop((move_handle, up_handle)), 0);
}

#[derive(Clone, Copy)]
struct TimelinePinchState {
    distance: f64,
    /// Pinch midpoint relative to the scroll container.
    anchor_viewport_x: f64,
    pixels_per_min: f64,
}

#[derive(Clone, Copy)]
struct ProgramPanState {
    pointer_x: i32,
    pointer_y: i32,
    scroll_left: i32,
    scroll_top: i32,
}

#[derive(Clone, Copy)]
struct TimelinePanState {
    pointer_x: i32,
    scroll_left: i32,
}

/// `(start, stop, start_window_min)` of the loaded EPG.
type EpgWindow = Option<(i64, i64, i64)>;

fn update_now_line(
    container_ref: &NodeRef,
    now_line_ref: &NodeRef,
    epg_window: EpgWindow,
    pixels_per_min: f64,
    recenter: bool,
) {
    let Some((start, stop, start_window_min)) = epg_window else { return };
    let (Some(div), Some(now_line)) = (container_ref.cast::<HtmlElement>(), now_line_ref.cast::<HtmlElement>()) else {
        return;
    };
    let now = Utc::now().timestamp();
    if now >= start && now <= stop {
        let now_line_pos = min_to_px(now / 60 - start_window_min, pixels_per_min);
        let _ = now_line.style().set_property("width", &format!("{now_line_pos}px"));
        let _ = now_line.style().set_property("display", "block");
        if recenter {
            let container_width = div.client_width();
            let scroll_pos = (now_line_pos as i32 - (container_width >> 1)).max(0);
            div.set_scroll_left(scroll_pos);
        }
    } else {
        let _ = now_line.style().set_property("display", "none");
    }
}

fn now_minute(start_window_min: i64) -> i64 { Utc::now().timestamp() / 60 - start_window_min }

fn recompute_viewport(
    container_ref: &NodeRef,
    channels_ref: &NodeRef,
    geometry: &RefCell<Geometry>,
    set_viewport: &UseStateSetter<Viewport>,
) {
    let Some(div) = container_ref.cast::<HtmlElement>() else { return };
    if let Some(channels) = channels_ref.cast::<HtmlElement>() {
        geometry.borrow_mut().channels_width = f64::from(channels.offset_width());
    }
    let scroll = ScrollState {
        scroll_top: f64::from(div.scroll_top()),
        scroll_left: f64::from(div.scroll_left()),
        client_height: f64::from(div.client_height()),
        client_width: f64::from(div.client_width()),
    };
    set_viewport.set(compute_viewport(&scroll, &geometry.borrow()));
}

/// One group load of a target.
struct GridLoad {
    target_id: u16,
    group: Arc<str>,
    channel_count: u32,
    filter: Option<EpgChannelFilter>,
}

/// Server-side form of the search; `None` when nothing is filtered.
fn epg_channel_filter(search: &SearchRequest) -> Option<EpgChannelFilter> {
    match search {
        SearchRequest::Clear => None,
        SearchRequest::Text(pattern, _) if pattern.trim().is_empty() => None,
        SearchRequest::Text(pattern, _) => Some(EpgChannelFilter::Text(pattern.clone())),
        SearchRequest::Regexp(pattern, _) => Some(EpgChannelFilter::Regexp(pattern.clone())),
    }
}

/// Recording source id: the playlist channel when known, else the EPG channel id, which the
/// backend resolves to a playlist channel.
fn pending_program_source_id(channel: &EpgGridChannel) -> String {
    channel.virtual_id.map_or_else(|| channel.epg_id.to_string(), |virtual_id| virtual_id.to_string())
}

fn recording_padding(config: Option<&AppConfigDto>) -> PaddingBounds {
    let rec = config.and_then(|cfg| cfg.config.video.as_ref()).and_then(|video| video.recording.as_ref());
    PaddingBounds {
        default_pre_roll_secs: rec.and_then(|c| c.default_pre_roll_secs).unwrap_or(0),
        max_pre_roll_secs: rec.map_or(900, |c| c.max_pre_roll_secs),
        default_post_roll_secs: rec.and_then(|c| c.default_post_roll_secs).unwrap_or(0),
        max_post_roll_secs: rec.map_or(1800, |c| c.max_post_roll_secs),
    }
}

#[component]
pub fn EpgView() -> Html {
    let services = use_service_context();
    let config_ctx = use_context::<ConfigContext>().expect("ConfigContext not found");
    let dialog = use_context::<DialogService>().expect("Dialog service not found");
    let translate = use_translation();
    let grid_model = use_state(|| None::<ModelRef>);
    let container_ref = use_node_ref();
    let channels_ref = use_node_ref();
    let grid_ref = use_node_ref();
    let now_line_ref = use_node_ref();
    let timeline_ref = use_node_ref();
    let pixels_per_min = use_state(|| TimelineZoom::new(DEFAULT_PIXELS_PER_MIN));
    // Zoom anchor not yet fully applied to the DOM. Yew may run effects later than the next
    // zoom frame, so the anchor (valid for every zoom) is kept until the target zoom is rendered.
    let pending_anchor = use_mut_ref(|| None::<ZoomAnchor>);
    // Recenter on the now line once the next loaded EPG has been rendered.
    let pending_recenter = use_mut_ref(|| false);
    // Minute ticker for the now line, with the EPG window it was started for.
    let now_ticker = use_mut_ref(|| None::<(EpgWindow, Interval)>);
    let pending_zoom = use_mut_ref(|| None::<(f64, f64)>);
    // Zoom the last zoom frame applied. Event handlers and the zoom frame read this instead of
    // the render snapshot, which lags behind when several wheel events arrive before a render.
    let zoom_target = use_mut_ref(|| DEFAULT_PIXELS_PER_MIN);
    let raf_id = use_mut_ref(|| None::<i32>);
    let raf_closure: RafClosure = use_mut_ref(|| None);
    let epg_request_seq = use_mut_ref(RequestSeq::default);
    let pinch_state = use_mut_ref(|| None::<TimelinePinchState>);
    let program_pan_state = use_mut_ref(|| None::<ProgramPanState>);
    let timeline_pan_state = use_mut_ref(|| None::<TimelinePanState>);
    let program_pan_listeners: PanListeners = use_mut_ref(|| None);
    let timeline_pan_listeners: PanListeners = use_mut_ref(|| None);
    let selected_epg_source = use_state(|| None::<PlaylistEpgRequest>);
    let search_filter = use_state::<SearchRequest, _>(|| SearchRequest::Clear);
    // Playlist groups of a target source; the grid shows one group at a time.
    let groups = use_state(|| Rc::new(Vec::<EpgGroupInfo>::new()));
    let selected_group = use_state(|| None::<(u16, Arc<str>)>);
    let groups_ready = use_state(|| false);
    let group_index_missing = use_state(|| false);
    // Group the user chose last; kept across searches that temporarily hide it.
    let last_group = use_mut_ref(|| None::<Arc<str>>);
    let geometry = use_mut_ref(|| Geometry {
        row_height: read_css_var_px("--epg-row-height", FALLBACK_ROW_HEIGHT_PX),
        header_height: read_css_var_px("--epg-header-height", FALLBACK_HEADER_HEIGHT_PX),
        channels_width: 0.0,
        pixels_per_min: DEFAULT_PIXELS_PER_MIN,
        total_rows: 0,
    });
    // Render copy of the row height; listeners read `geometry`, the render reads this.
    let row_height = use_state_eq(|| geometry.borrow().row_height);
    let viewport = use_state_eq(Viewport::default);
    let now_min = use_state_eq(|| 0i64);
    let can_write_recordings = services.auth.has_permission(Permission::RecordingCreate);
    let is_admin_role = services.auth.is_admin();
    let is_hosted_epg = matches!(*selected_epg_source, Some(PlaylistEpgRequest::Target(_)));

    let filtered = use_memo(((*grid_model).clone(), (*search_filter).clone()), |(model, filter)| {
        model.as_ref().map_or_else(Vec::new, |m| filter_channel_indices(&m.0, filter))
    });
    let timeline_zoom = *pixels_per_min;
    {
        let mut geo = geometry.borrow_mut();
        geo.pixels_per_min = timeline_zoom.value();
        geo.total_rows = filtered.len();
    }

    let recompute = {
        let container_ref = container_ref.clone();
        let channels_ref = channels_ref.clone();
        let geometry = geometry.clone();
        let set_viewport = viewport.setter();
        Rc::new(move || recompute_viewport(&container_ref, &channels_ref, &geometry, &set_viewport))
    };

    // Clears data, scroll and zoom state shared by every new load.
    let reset_view = {
        let grid_model = grid_model.clone();
        let container_ref = container_ref.clone();
        let pending_anchor = pending_anchor.clone();
        let pending_zoom = pending_zoom.clone();
        let raf_id = raf_id.clone();
        let raf_closure = raf_closure.clone();
        let viewport = viewport.clone();
        Rc::new(move || {
            grid_model.set(None);
            viewport.set(Viewport::default());
            *pending_anchor.borrow_mut() = None;
            *pending_zoom.borrow_mut() = None;
            if let Some(id) = raf_id.borrow_mut().take() {
                if let Some(win) = window() {
                    let _ = win.cancel_animation_frame(id);
                }
            }
            *raf_closure.borrow_mut() = None;
            if let Some(el) = container_ref.cast::<HtmlElement>() {
                el.set_scroll_top(0);
                el.set_scroll_left(0);
            }
        })
    };

    // Loads one group of a target with the current search applied on the server.
    let load_grid = {
        let service_ctx = services.clone();
        let translate = translate.clone();
        let grid_model = grid_model.clone();
        let epg_request_seq = epg_request_seq.clone();
        let pending_recenter = pending_recenter.clone();
        let reset_view = reset_view.clone();
        Callback::from(move |request: GridLoad| {
            reset_view();
            let token = epg_request_seq.borrow_mut().begin();
            if request.channel_count as usize > MAX_EPG_GRID_ROWS {
                service_ctx.toastr.warning(translate.t("MESSAGES.EPG.GROUP_TRUNCATED"));
            }
            let service_ctx = service_ctx.clone();
            let translate = translate.clone();
            let grid_model = grid_model.clone();
            let epg_request_seq = epg_request_seq.clone();
            let pending_recenter = pending_recenter.clone();
            service_ctx.event.broadcast(EventMessage::Busy(BusyStatus::Show));
            spawn_local(async move {
                let rows = service_ctx
                    .playlist
                    .get_epg_grid(request.target_id, request.group.to_string(), request.filter)
                    .await;
                // The timeout lets the busy indicator paint before the model build blocks the thread.
                set_timeout(
                    move || {
                        finish_load(
                            &epg_request_seq.borrow(),
                            token,
                            rows,
                            |rows| match rows {
                                Ok(rows) => {
                                    *pending_recenter.borrow_mut() = rows.is_some();
                                    grid_model.set(rows.map(|rows| {
                                        ModelRef(Rc::new(EpgGridModel::from_grid_rows(
                                            &rows,
                                            cached_offsets(local_offset_secs),
                                        )))
                                    }));
                                }
                                Err(err) => {
                                    *pending_recenter.borrow_mut() = false;
                                    grid_model.set(None);
                                    service_ctx.toastr.error(format!("{}: {err}", translate.t("LABEL.EPG")));
                                }
                            },
                            || service_ctx.event.broadcast(EventMessage::Busy(BusyStatus::Hide)),
                        );
                    },
                    16,
                );
            });
        })
    };

    // Loads the groups of a target that have channels matching the search, then the grid of the
    // preferred group if it is still listed, else of the first one.
    let load_groups = {
        let service_ctx = services.clone();
        let translate = translate.clone();
        let grid_model = grid_model.clone();
        let epg_request_seq = epg_request_seq.clone();
        let groups = groups.clone();
        let selected_group = selected_group.clone();
        let groups_ready = groups_ready.clone();
        let group_index_missing = group_index_missing.clone();
        let last_group = last_group.clone();
        let load_grid = load_grid.clone();
        Callback::from(move |(target_id, filter, preferred): (u16, Option<EpgChannelFilter>, Option<Arc<str>>)| {
            let token = epg_request_seq.borrow_mut().begin();
            let service_ctx = service_ctx.clone();
            let translate = translate.clone();
            let grid_model = grid_model.clone();
            let epg_request_seq = epg_request_seq.clone();
            let groups = groups.clone();
            let selected_group = selected_group.clone();
            let groups_ready = groups_ready.clone();
            let group_index_missing = group_index_missing.clone();
            let last_group = last_group.clone();
            let load_grid = load_grid.clone();
            // The listed groups belong to the previous request until the new list arrives.
            groups_ready.set(false);
            service_ctx.event.broadcast(EventMessage::Busy(BusyStatus::Show));
            spawn_local(async move {
                let result = service_ctx.playlist.get_epg_groups(target_id, filter.clone()).await;
                let mut next_grid = None;
                finish_load(
                    &epg_request_seq.borrow(),
                    token,
                    result,
                    |result| match result {
                        Ok(Some(list)) => {
                            let chosen = preferred
                                .as_ref()
                                .and_then(|name| list.iter().find(|group| &group.name == name))
                                .or_else(|| list.first());
                            next_grid = chosen.map(|group| GridLoad {
                                target_id,
                                group: Arc::clone(&group.name),
                                channel_count: group.channel_count,
                                filter: filter.clone(),
                            });
                            selected_group.set(next_grid.as_ref().map(|load| (target_id, Arc::clone(&load.group))));
                            // Without a search the shown group is the one to return to after a search.
                            if filter.is_none() {
                                *last_group.borrow_mut() = next_grid.as_ref().map(|load| Arc::clone(&load.group));
                            }
                            if next_grid.is_none() {
                                // The search matched no channel: nothing to show.
                                grid_model.set(None);
                            }
                            groups.set(Rc::new(list));
                            groups_ready.set(true);
                        }
                        Ok(None) => group_index_missing.set(true),
                        Err(err) => {
                            groups_ready.set(true);
                            service_ctx.toastr.error(format!("{}: {err}", translate.t("LABEL.EPG_GROUPS")));
                        }
                    },
                    || service_ctx.event.broadcast(EventMessage::Busy(BusyStatus::Hide)),
                );
                // Started after `finish_load`, which holds the request sequence borrowed.
                if let Some(next_grid) = next_grid {
                    load_grid.emit(next_grid);
                }
            });
        })
    };

    let handle_search = {
        let search_filter = search_filter.clone();
        let container_ref = container_ref.clone();
        let recompute = recompute.clone();
        let load_groups = load_groups.clone();
        let last_group = last_group.clone();
        let group_target = match *selected_epg_source {
            Some(PlaylistEpgRequest::Target(target_id)) if !*group_index_missing => Some(target_id),
            _ => None,
        };
        Callback::from(move |req: SearchRequest| {
            let filter = epg_channel_filter(&req);
            search_filter.set(req);
            if let Some(target_id) = group_target {
                // Grouped targets search on the server across all groups, so groups without a
                // match disappear from the list.
                load_groups.emit((target_id, filter, last_group.borrow().clone()));
                return;
            }
            if let Some(el) = container_ref.cast::<HtmlElement>() {
                el.set_scroll_top(0);
            }
            recompute();
        })
    };

    let handle_select_source = {
        let service_ctx = services.clone();
        let grid_model = grid_model.clone();
        let search_filter = search_filter.clone();
        let pixels_per_min = pixels_per_min.clone();
        let pending_recenter = pending_recenter.clone();
        let zoom_target = zoom_target.clone();
        let epg_request_seq = epg_request_seq.clone();
        let selected_epg_source = selected_epg_source.clone();
        let groups = groups.clone();
        let selected_group = selected_group.clone();
        let groups_ready = groups_ready.clone();
        let group_index_missing = group_index_missing.clone();
        let last_group = last_group.clone();
        let reset_view = reset_view.clone();
        let load_groups = load_groups.clone();
        Callback::from(move |req: PlaylistEpgRequest| {
            // Everything of the previous source goes first, so its groups can never be selected
            // for the new source and its in-flight requests are superseded.
            selected_epg_source.set(Some(req.clone()));
            search_filter.set(SearchRequest::Clear);
            groups.set(Rc::new(Vec::new()));
            selected_group.set(None);
            groups_ready.set(false);
            group_index_missing.set(false);
            *last_group.borrow_mut() = None;
            pixels_per_min.set(TimelineZoom::new(DEFAULT_PIXELS_PER_MIN));
            *zoom_target.borrow_mut() = DEFAULT_PIXELS_PER_MIN;
            reset_view();

            if let PlaylistEpgRequest::Target(target_id) = req {
                load_groups.emit((target_id, None, None));
                return;
            }

            let token = epg_request_seq.borrow_mut().begin();
            let service_ctx = service_ctx.clone();
            let epg_request_seq = epg_request_seq.clone();
            service_ctx.event.broadcast(EventMessage::Busy(BusyStatus::Show));
            let grid_model = grid_model.clone();
            let pending_recenter = pending_recenter.clone();
            spawn_local(async move {
                let playlist_epg = service_ctx.playlist.get_playlist_epg(req).await;
                // The timeout lets the busy indicator paint before the model build blocks the thread.
                set_timeout(
                    move || {
                        finish_load(
                            &epg_request_seq.borrow(),
                            token,
                            playlist_epg,
                            |tv| {
                                *pending_recenter.borrow_mut() = tv.is_some();
                                grid_model.set(tv.map(|tv| {
                                    ModelRef(Rc::new(EpgGridModel::from_epg_tv(&tv, cached_offsets(local_offset_secs))))
                                }));
                            },
                            || service_ctx.event.broadcast(EventMessage::Busy(BusyStatus::Hide)),
                        );
                    },
                    16,
                );
            });
        })
    };

    let handle_select_group = {
        let groups = groups.clone();
        let selected_group = selected_group.clone();
        let groups_ready = groups_ready.clone();
        let last_group = last_group.clone();
        let load_grid = load_grid.clone();
        let filter = epg_channel_filter(&search_filter);
        Callback::from(move |group: Arc<str>| {
            if !*groups_ready {
                return;
            }
            // The target comes from the current selection, never from a separately read source.
            let Some((target_id, current)) = (*selected_group).clone() else { return };
            if current == group {
                return;
            }
            let channel_count = groups.iter().find(|g| g.name == group).map_or(0, |g| g.channel_count);
            selected_group.set(Some((target_id, Arc::clone(&group))));
            *last_group.borrow_mut() = Some(Arc::clone(&group));
            load_grid.emit(GridLoad { target_id, group, channel_count, filter: filter.clone() });
        })
    };

    let epg_window: EpgWindow = (*grid_model).as_ref().map(|m| (m.0.start, m.0.stop, m.0.start_window_min));
    let num_blocks = (*grid_model).as_ref().map_or(0, |m| m.0.num_blocks);
    let start_window_min = epg_window.map_or(0, |(_, _, start_window_min)| start_window_min);

    // Block labels only change with the data; the zoom only changes their width.
    let timeline_labels = use_memo((*grid_model).clone(), move |_| {
        (0..num_blocks)
            .map(|i| {
                let block_secs = (start_window_min + i * TIME_BLOCK_MINS).saturating_mul(60);
                Utc.timestamp_opt(block_secs, 0).single().map(|utc| {
                    let local = utc.with_timezone(&Local);
                    (local.format("%H:%M").to_string(), format!("{:02}.{:02}", local.day(), local.month()))
                })
            })
            .collect::<Vec<_>>()
    });

    let timeline_html = use_memo(((*grid_model).clone(), timeline_zoom), move |(_, zoom)| {
        let labels = &timeline_labels;
        if labels.is_empty() {
            return html! {};
        }
        let block_width = zoom.value() * TIME_BLOCK_MINS as f64;
        let block_style = format!("width:{block_width}px; min-width:{block_width}px; max-width:{block_width}px");
        html! {
            <div class="tp__epg__timeline">
                { for labels.iter().map(|label| match label {
                    Some((hour_min, day_month)) => html! {
                        <div class="tp__epg__timeline-block" style={block_style.clone()}>
                            <div class="tp__epg__timeline-block-time">{ hour_min }</div>
                            <div class="tp__epg__timeline-block-date">{ day_month }</div>
                        </div>
                    },
                    None => html! { <div class="tp__epg__timeline-block" style={block_style.clone()}></div> },
                }) }
            </div>
        }
    });

    {
        let raf_id = raf_id.clone();
        let raf_closure = raf_closure.clone();
        let program_pan_listeners = program_pan_listeners.clone();
        let timeline_pan_listeners = timeline_pan_listeners.clone();
        let now_ticker = now_ticker.clone();
        use_effect_with((), move |()| {
            move || {
                now_ticker.borrow_mut().take();
                if let Some(id) = raf_id.borrow_mut().take() {
                    if let Some(win) = window() {
                        let _ = win.cancel_animation_frame(id);
                    }
                }
                *raf_closure.borrow_mut() = None;
                end_pan(&program_pan_listeners, None, PROGRAM_GRID_PANNING_CLASS);
                end_pan(&timeline_pan_listeners, None, TIMELINE_PANNING_CLASS);
            }
        });
    }

    // Pending DOM work after every render. Dependency-keyed effects are not used here: Yew may
    // run them only after the next zoom frame, so each step checks what this render needs.
    {
        let container_ref = container_ref.clone();
        let channels_ref = channels_ref.clone();
        let now_line_ref = now_line_ref.clone();
        let pending_anchor = pending_anchor.clone();
        let pending_recenter = pending_recenter.clone();
        let zoom_target = zoom_target.clone();
        let now_ticker = now_ticker.clone();
        let set_now_min = now_min.setter();
        let recompute = recompute.clone();
        let rendered_zoom = timeline_zoom.value();
        use_effect(move || {
            // 1. Keep the zoom anchor under the pointer for the zoom this render shows.
            let anchor = *pending_anchor.borrow();
            if let (Some(anchor), Some(container)) = (anchor, container_ref.cast::<HtmlElement>()) {
                let channels_width = channels_ref.cast::<HtmlElement>().map_or(0.0, |c| f64::from(c.offset_width()));
                let scroll_left = anchor_scroll_left(anchor, channels_width, rendered_zoom).round() as i32;
                container.set_scroll_left(scroll_left);
                if (rendered_zoom - *zoom_target.borrow()).abs() < ZOOM_EQUALITY_TOLERANCE {
                    *pending_anchor.borrow_mut() = None;
                }
            }

            // 2. Now line, recenter after a load, minute ticker per loaded EPG.
            let recenter = epg_window.is_some() && std::mem::take(&mut *pending_recenter.borrow_mut());
            update_now_line(&container_ref, &now_line_ref, epg_window, rendered_zoom, recenter);
            let ticker_window = now_ticker.borrow().as_ref().map(|(window, _)| *window);
            if ticker_window != Some(epg_window) {
                if let Some((_, _, start_window_min)) = epg_window {
                    set_now_min.set(now_minute(start_window_min));
                }
                let interval = {
                    let container_ref = container_ref.clone();
                    let now_line_ref = now_line_ref.clone();
                    let zoom_target = zoom_target.clone();
                    let set_now_min = set_now_min.clone();
                    Interval::new(60_000, move || {
                        update_now_line(&container_ref, &now_line_ref, epg_window, *zoom_target.borrow(), false);
                        if let Some((_, _, start_window_min)) = epg_window {
                            set_now_min.set(now_minute(start_window_min));
                        }
                    })
                };
                *now_ticker.borrow_mut() = Some((epg_window, interval));
            }

            // 3. Rendered rows and minutes for the current scroll, zoom and filter.
            recompute();
            || ()
        });
    }

    // Scroll: one viewport update per animation frame.
    {
        let container_ref = container_ref.clone();
        let recompute = recompute.clone();
        use_effect_with((), move |()| {
            let pending_frame = Rc::new(Cell::new(None::<i32>));
            let frame_closure: Rc<Closure<dyn FnMut(f64)>> = {
                let pending_frame = pending_frame.clone();
                Rc::new(Closure::wrap(Box::new(move |_ts: f64| {
                    pending_frame.set(None);
                    recompute();
                }) as Box<dyn FnMut(f64)>))
            };
            let onscroll = {
                let pending_frame = pending_frame.clone();
                let frame_closure = frame_closure.clone();
                Closure::wrap(Box::new(move |_event: web_sys::Event| {
                    if pending_frame.get().is_some() {
                        return;
                    }
                    if let Some(win) = window() {
                        if let Ok(id) = win.request_animation_frame((*frame_closure).as_ref().unchecked_ref()) {
                            pending_frame.set(Some(id));
                        }
                    }
                }) as Box<dyn FnMut(web_sys::Event)>)
            };
            if let Some(div) = container_ref.cast::<HtmlElement>() {
                if let Err(err) = div.add_event_listener_with_callback("scroll", onscroll.as_ref().unchecked_ref()) {
                    log::error!("Failed to register EPG scroll listener: {err:?}");
                }
            }
            move || {
                if let Some(id) = pending_frame.take() {
                    if let Some(win) = window() {
                        let _ = win.cancel_animation_frame(id);
                    }
                }
                // Detach before dropping the closure so a live element cannot invoke a destroyed callback
                if let Some(div) = container_ref.cast::<HtmlElement>() {
                    let _ = div.remove_event_listener_with_callback("scroll", onscroll.as_ref().unchecked_ref());
                }
                drop(onscroll);
                drop(frame_closure);
            }
        });
    }

    // Resize (including the mobile breakpoint): re-measure, then recompute with the new values.
    {
        let geometry = geometry.clone();
        let set_row_height = row_height.setter();
        let recompute = recompute.clone();
        use_effect_with((), move |()| {
            let onresize = Closure::wrap(Box::new(move |_event: web_sys::Event| {
                let row_height = read_css_var_px("--epg-row-height", FALLBACK_ROW_HEIGHT_PX);
                {
                    let mut geo = geometry.borrow_mut();
                    geo.row_height = row_height;
                    geo.header_height = read_css_var_px("--epg-header-height", FALLBACK_HEADER_HEIGHT_PX);
                }
                set_row_height.set(row_height);
                recompute();
            }) as Box<dyn FnMut(web_sys::Event)>);
            if let Some(win) = window() {
                let _ = win.add_event_listener_with_callback("resize", onresize.as_ref().unchecked_ref());
            }
            move || {
                if let Some(win) = window() {
                    let _ = win.remove_event_listener_with_callback("resize", onresize.as_ref().unchecked_ref());
                }
                drop(onresize);
            }
        });
    }

    let apply_timeline_zoom = {
        let pending_zoom = pending_zoom.clone();
        let zoom_target = zoom_target.clone();
        let raf_id = raf_id.clone();
        let raf_closure = raf_closure.clone();
        let pixels_per_min = pixels_per_min.clone();
        let container_ref = container_ref.clone();
        let channels_ref = channels_ref.clone();
        let pending_anchor = pending_anchor.clone();
        // Input: (target zoom, pointer x relative to the scroll container).
        Callback::from(move |(next_pixels_per_min, anchor_viewport_x): (f64, f64)| {
            let next_pixels_per_min = TimelineZoom::new(next_pixels_per_min).value();
            *pending_zoom.borrow_mut() = Some((next_pixels_per_min, anchor_viewport_x));

            // If RAF already scheduled, it will pick up the latest pending_zoom
            if raf_id.borrow().is_some() {
                return;
            }

            let Some(win) = window() else { return };

            let pending_zoom_r = pending_zoom.clone();
            let zoom_target_r = zoom_target.clone();
            let raf_id_r = raf_id.clone();
            let pixels_per_min_r = pixels_per_min.clone();
            let container_ref_r = container_ref.clone();
            let channels_ref_r = channels_ref.clone();
            let pending_anchor_r = pending_anchor.clone();

            let closure = Closure::wrap(Box::new(move |_ts: f64| {
                *raf_id_r.borrow_mut() = None;
                let Some((next_ppm, anchor_viewport_x)) = pending_zoom_r.borrow_mut().take() else {
                    return;
                };
                let current_ppm = *zoom_target_r.borrow();
                if (next_ppm - current_ppm).abs() < ZOOM_EQUALITY_TOLERANCE {
                    return;
                }
                // The minute under the pointer comes from the pending anchor while the DOM has not
                // caught up with the last zoom; only a settled DOM is measured.
                let pending = *pending_anchor_r.borrow();
                let anchor = if let Some(pending) = pending {
                    shift_anchor(pending, anchor_viewport_x, current_ppm)
                } else {
                    let scroll_left = container_ref_r.cast::<HtmlElement>().map_or(0.0, |c| f64::from(c.scroll_left()));
                    let channels_width =
                        channels_ref_r.cast::<HtmlElement>().map_or(0.0, |c| f64::from(c.offset_width()));
                    ZoomAnchor {
                        viewport_x: anchor_viewport_x,
                        minute: minute_at(scroll_left, anchor_viewport_x, channels_width, current_ppm),
                    }
                };
                *zoom_target_r.borrow_mut() = next_ppm;
                *pending_anchor_r.borrow_mut() = Some(anchor);
                pixels_per_min_r.set(TimelineZoom::new(next_ppm));
            }) as Box<dyn FnMut(f64)>);

            if let Ok(id) = win.request_animation_frame(closure.as_ref().unchecked_ref()) {
                *raf_id.borrow_mut() = Some(id);
                *raf_closure.borrow_mut() = Some(closure);
            }
        })
    };

    let handle_timeline_wheel = {
        let container_ref = container_ref.clone();
        let apply_timeline_zoom = apply_timeline_zoom.clone();
        let pending_zoom = pending_zoom.clone();
        let zoom_target = zoom_target.clone();
        Callback::from(move |e: WheelEvent| {
            // The time header is the only place the wheel zooms; elsewhere, and for horizontal
            // wheel or trackpad scrolls on the header, the container scrolls natively.
            let Some(container) = container_ref.cast::<HtmlElement>() else { return };
            let Some(factor) =
                wheel_zoom_factor(e.delta_x(), e.delta_y(), e.delta_mode(), f64::from(container.client_height()))
            else {
                return;
            };
            e.prevent_default();
            e.stop_propagation();
            let anchor_viewport_x = f64::from(e.client_x()) - container.get_bounding_client_rect().left();
            // Several wheel events within one frame accumulate on the not yet applied zoom.
            let base = pending_zoom.borrow().map_or_else(|| *zoom_target.borrow(), |(zoom, _)| zoom);
            apply_timeline_zoom.emit((base * factor, anchor_viewport_x));
        })
    };

    let handle_timeline_touch_start = {
        let container_ref = container_ref.clone();
        let pinch_state = pinch_state.clone();
        let timeline_pan_state = timeline_pan_state.clone();
        let zoom_target = zoom_target.clone();
        Callback::from(move |e: TouchEvent| {
            let touch_count = e.touches().length();
            if touch_count == 2 {
                let Some(first) = e.touches().item(0) else {
                    return;
                };
                let Some(second) = e.touches().item(1) else {
                    return;
                };
                let Some(container) = container_ref.cast::<HtmlElement>() else {
                    return;
                };

                e.prevent_default();
                e.stop_propagation();

                *timeline_pan_state.borrow_mut() = None;

                let rect = container.get_bounding_client_rect();
                let dx = f64::from(second.client_x() - first.client_x());
                let dy = f64::from(second.client_y() - first.client_y());
                let distance = (dx * dx + dy * dy).sqrt();
                let midpoint_x = f64::midpoint(f64::from(first.client_x()), f64::from(second.client_x()));

                *pinch_state.borrow_mut() = Some(TimelinePinchState {
                    distance,
                    anchor_viewport_x: midpoint_x - rect.left(),
                    pixels_per_min: *zoom_target.borrow(),
                });
            } else if touch_count == 1 {
                let Some(container) = container_ref.cast::<HtmlElement>() else {
                    return;
                };
                let Some(touch) = e.touches().item(0) else {
                    return;
                };
                e.prevent_default();
                e.stop_propagation();
                *pinch_state.borrow_mut() = None;
                *timeline_pan_state.borrow_mut() =
                    Some(TimelinePanState { pointer_x: touch.client_x(), scroll_left: container.scroll_left() });
            }
        })
    };

    let handle_timeline_touch_move = {
        let pinch_state = pinch_state.clone();
        let timeline_pan_state = timeline_pan_state.clone();
        let container_ref = container_ref.clone();
        let apply_timeline_zoom = apply_timeline_zoom.clone();
        Callback::from(move |e: TouchEvent| {
            if e.touches().length() == 2 {
                let Some(initial) = *pinch_state.borrow() else {
                    return;
                };
                let Some(first) = e.touches().item(0) else {
                    return;
                };
                let Some(second) = e.touches().item(1) else {
                    return;
                };

                e.prevent_default();
                e.stop_propagation();

                let dx = f64::from(second.client_x() - first.client_x());
                let dy = f64::from(second.client_y() - first.client_y());
                let distance = (dx * dx + dy * dy).sqrt();
                if initial.distance <= 0.0 {
                    return;
                }

                apply_timeline_zoom
                    .emit((initial.pixels_per_min * (distance / initial.distance), initial.anchor_viewport_x));
            } else if e.touches().length() == 1 {
                let Some(pan_state) = *timeline_pan_state.borrow() else {
                    return;
                };
                let Some(container) = container_ref.cast::<HtmlElement>() else {
                    return;
                };
                let Some(touch) = e.touches().item(0) else {
                    return;
                };
                e.prevent_default();
                e.stop_propagation();
                container.set_scroll_left(compute_panned_scroll(
                    pan_state.scroll_left,
                    pan_state.pointer_x,
                    touch.client_x(),
                ));
            }
        })
    };

    let handle_timeline_touch_end = {
        let pinch_state = pinch_state.clone();
        let timeline_pan_state = timeline_pan_state.clone();
        Callback::from(move |_e: TouchEvent| {
            *pinch_state.borrow_mut() = None;
            *timeline_pan_state.borrow_mut() = None;
        })
    };

    // Mouse panning keeps its state in refs and toggles the cursor class directly, so a click
    // on the grid never re-renders the view.
    let handle_timeline_mouse_down = {
        let container_ref = container_ref.clone();
        let timeline_ref = timeline_ref.clone();
        let timeline_pan_state = timeline_pan_state.clone();
        let listeners = timeline_pan_listeners.clone();
        Callback::from(move |e: MouseEvent| {
            if e.button() != 0 {
                return;
            }
            let Some(container) = container_ref.cast::<HtmlElement>() else {
                return;
            };
            e.prevent_default();
            e.stop_propagation();
            end_pan(&listeners, timeline_ref.cast::<HtmlElement>(), TIMELINE_PANNING_CLASS);
            *timeline_pan_state.borrow_mut() =
                Some(TimelinePanState { pointer_x: e.client_x(), scroll_left: container.scroll_left() });
            if let Some(timeline) = timeline_ref.cast::<HtmlElement>() {
                let _ = timeline.class_list().add_1(TIMELINE_PANNING_CLASS);
            }
            let on_move = {
                let container_ref = container_ref.clone();
                let timeline_ref = timeline_ref.clone();
                let timeline_pan_state = timeline_pan_state.clone();
                let listeners = listeners.clone();
                Box::new(move |e: MouseEvent| {
                    if !is_primary_button_held(&e) {
                        *timeline_pan_state.borrow_mut() = None;
                        end_pan(&listeners, timeline_ref.cast::<HtmlElement>(), TIMELINE_PANNING_CLASS);
                        return;
                    }
                    let Some(pan_state) = *timeline_pan_state.borrow() else { return };
                    let Some(container) = container_ref.cast::<HtmlElement>() else { return };
                    e.prevent_default();
                    container.set_scroll_left(compute_panned_scroll(
                        pan_state.scroll_left,
                        pan_state.pointer_x,
                        e.client_x(),
                    ));
                }) as Box<dyn FnMut(MouseEvent)>
            };
            let on_up = {
                let timeline_ref = timeline_ref.clone();
                let timeline_pan_state = timeline_pan_state.clone();
                let listeners = listeners.clone();
                Box::new(move |_e: MouseEvent| {
                    *timeline_pan_state.borrow_mut() = None;
                    end_pan(&listeners, timeline_ref.cast::<HtmlElement>(), TIMELINE_PANNING_CLASS);
                }) as Box<dyn FnMut(MouseEvent)>
            };
            *listeners.borrow_mut() = register_mouse_pan(on_move, on_up);
        })
    };

    let handle_programs_mouse_down = {
        let container_ref = container_ref.clone();
        let grid_ref = grid_ref.clone();
        let program_pan_state = program_pan_state.clone();
        let listeners = program_pan_listeners.clone();
        Callback::from(move |e: MouseEvent| {
            if e.button() != 0 {
                return;
            }
            let Some(container) = container_ref.cast::<HtmlElement>() else {
                return;
            };
            e.prevent_default();
            e.stop_propagation();
            end_pan(&listeners, grid_ref.cast::<HtmlElement>(), PROGRAM_GRID_PANNING_CLASS);
            *program_pan_state.borrow_mut() = Some(ProgramPanState {
                pointer_x: e.client_x(),
                pointer_y: e.client_y(),
                scroll_left: container.scroll_left(),
                scroll_top: container.scroll_top(),
            });
            if let Some(grid) = grid_ref.cast::<HtmlElement>() {
                let _ = grid.class_list().add_1(PROGRAM_GRID_PANNING_CLASS);
            }
            let on_move = {
                let container_ref = container_ref.clone();
                let grid_ref = grid_ref.clone();
                let program_pan_state = program_pan_state.clone();
                let listeners = listeners.clone();
                Box::new(move |e: MouseEvent| {
                    if !is_primary_button_held(&e) {
                        *program_pan_state.borrow_mut() = None;
                        end_pan(&listeners, grid_ref.cast::<HtmlElement>(), PROGRAM_GRID_PANNING_CLASS);
                        return;
                    }
                    let Some(pan_state) = *program_pan_state.borrow() else { return };
                    let Some(container) = container_ref.cast::<HtmlElement>() else { return };
                    e.prevent_default();
                    container.set_scroll_left(compute_panned_scroll(
                        pan_state.scroll_left,
                        pan_state.pointer_x,
                        e.client_x(),
                    ));
                    container.set_scroll_top(compute_panned_scroll(
                        pan_state.scroll_top,
                        pan_state.pointer_y,
                        e.client_y(),
                    ));
                }) as Box<dyn FnMut(MouseEvent)>
            };
            let on_up = {
                let grid_ref = grid_ref.clone();
                let program_pan_state = program_pan_state.clone();
                let listeners = listeners.clone();
                Box::new(move |_e: MouseEvent| {
                    *program_pan_state.borrow_mut() = None;
                    end_pan(&listeners, grid_ref.cast::<HtmlElement>(), PROGRAM_GRID_PANNING_CLASS);
                }) as Box<dyn FnMut(MouseEvent)>
            };
            *listeners.borrow_mut() = register_mouse_pan(on_move, on_up);
        })
    };

    // The record callback is rebuilt only when one of its inputs changes, so `EpgRow`s keep
    // equal props across renders. Config and translation are compared by identity.
    let handle_record = {
        let dialog = dialog.clone();
        let services = services.clone();
        let config = config_ctx.config.clone();
        let translate_for_body = translate.clone();
        use_callback(
            (
                (*grid_model).clone(),
                (*selected_epg_source).clone(),
                config_ctx.config.as_ref().map(Rc::as_ptr),
                translate.clone(),
                can_write_recordings,
                is_admin_role,
            ),
            move |(key, idx): (usize, usize), deps| {
                let (model, source, _, _, can_write_recordings, is_admin_role) = deps;
                let translate = translate_for_body.clone();
                let Some(PlaylistEpgRequest::Target(target_id)) = source.clone() else {
                    services.toastr.error(translate.t("MESSAGES.RECORDING.NO_TARGET"));
                    return;
                };
                let Some(target_name) =
                    config.as_ref().and_then(|app_config| target_name_for_id(&app_config.sources, target_id, None))
                else {
                    services.toastr.error(translate.t("MESSAGES.RECORDING.NO_TARGET"));
                    return;
                };
                let Some(channel) = model.as_ref().and_then(|m| m.0.channels.get(key)).cloned() else { return };
                let Some(programme) = channel.programmes.get(idx) else { return };
                let channel_name = (!channel.title.is_empty()).then(|| channel.title.to_string());
                let programme_title = programme.title.to_string();
                let (programme_start, programme_end) = (programme.start, programme.stop);
                let padding = recording_padding(config.as_deref());
                let (can_write_recordings, is_admin_role) = (*can_write_recordings, *is_admin_role);
                let dialog = dialog.clone();
                let services = services.clone();
                spawn_local(async move {
                    if !ensure_recording_available(&services, &translate).await {
                        return;
                    }
                    let source = RecordingSourceInput {
                        target_id: target_name,
                        virtual_id: pending_program_source_id(&channel),
                        cluster: XtreamCluster::Live,
                        input_name: String::new(),
                    };
                    let mut prefill = epg_programme_to_prefill(EpgProgrammePrefillInput {
                        source,
                        channel_id: Some(channel.epg_id.to_string()),
                        channel_name: channel_name.clone(),
                        programme_title,
                        programme_start,
                        programme_end,
                        padding,
                        episode: None,
                    });
                    if let Some(name) = channel_name {
                        prefill = prefill.with_channel_name(name);
                    }
                    let request_slot: Rc<RefCell<Option<CreateRecordingTaskRequest>>> = Rc::new(RefCell::new(None));
                    let on_submit = {
                        let request_slot = Rc::clone(&request_slot);
                        Callback::from(move |request: CreateRecordingTaskRequest| {
                            *request_slot.borrow_mut() = Some(request);
                        })
                    };
                    let body = html! {
                        <RecordingForm
                            prefill={prefill}
                            has_recording_manage={can_write_recordings}
                            is_admin_role={is_admin_role}
                            on_submit={on_submit}
                            on_cancel={Callback::from(|()| {})}
                        />
                    };
                    let actions = DialogActions {
                        left: Some(vec![DialogAction::new(
                            "cancel",
                            "LABEL.CANCEL",
                            DialogResult::Cancel,
                            Some("Close".to_owned()),
                            None,
                        )]),
                        right: vec![DialogAction::new_focused(
                            "record",
                            "LABEL.RECORD",
                            DialogResult::Ok,
                            Some("Record".to_owned()),
                            Some("primary".to_string()),
                        )],
                    };
                    if dialog.content(body, Some(actions), false).await != DialogResult::Ok {
                        return;
                    }
                    let Some(request) = request_slot.borrow_mut().take() else {
                        services.toastr.error(translate.t("MESSAGES.RECORDING.NO_REQUEST"));
                        return;
                    };
                    match RecordingService::new().create_task(request).await {
                        Ok(()) => services.toastr.success(translate.t("MESSAGES.RECORDING.QUEUED")),
                        Err(err) => services.toastr.error(err.to_string()),
                    }
                });
            },
        )
    };

    let body = match (*grid_model).as_ref() {
        None if *group_index_missing => html! {
            <NoContent text={translate.t("MESSAGES.EPG.UPDATE_TARGET_FOR_GROUPS")} />
        },
        None if *groups_ready && groups.is_empty() && epg_channel_filter(&search_filter).is_some() => html! {
            <NoContent text={translate.t("MESSAGES.EPG.NO_SEARCH_MATCHES")} />
        },
        None if *groups_ready && groups.is_empty() => html! {
            <NoContent text={translate.t("MESSAGES.EPG.NO_GROUPS_WITH_EPG")} />
        },
        None if selected_epg_source.is_some() => html! {},
        None => html! {
            <NoContent text={translate.t("MESSAGES.EPG.SELECT_AN_EPG_TO_VIEW_CONTENT")} hint={translate.t("MESSAGES.EPG.SELECT_AN_EPG_HINT")}/>
        },
        Some(model) => {
            let model = &model.0;
            let ppm = timeline_zoom.value();
            let rh = *row_height;
            let total = filtered.len();
            let rows = viewport.row_start.min(total)..viewport.row_end.min(total);
            let top_spacer = format!("height:{}px", rows.start as f64 * rh);
            let bottom_spacer = format!("height:{}px", (total - rows.end) as f64 * rh);
            let content_width = min_to_px(model.num_blocks * TIME_BLOCK_MINS, ppm);
            let content_style =
                format!("width:{content_width}px; min-width:{content_width}px; max-width:{content_width}px");
            let channel_style = format!("max-height:{rh}px;min-height:{rh}px;height:{rh}px");
            let row_style = AttrValue::from(format!("{channel_style};width:{content_width}px"));
            let can_record = can_write_recordings && is_hosted_epg;
            let visible = &filtered[rows];
            html! {
                <>
                <div class="tp__epg__channels" ref={channels_ref.clone()}>
                    <div class="tp__epg__channels-header"></div>
                    <div style={top_spacer.clone()}></div>
                    { for visible.iter().map(|&idx| {
                        let ch = &model.channels[idx];
                        html! {
                            <div key={ch.key} class="tp__epg__channel" title={concat_string!(&ch.title, " (", &ch.epg_id, ")")}
                                 style={channel_style.clone()}>
                                <div class="tp__epg__channel-icon">
                                    { if let Some(icon) = &ch.icon {
                                        html! { <img src={icon.to_string()} alt={ch.title.to_string()} /> }
                                      } else { html!{} }
                                    }
                                </div>
                                <div class="tp__epg__channel-title">{ &*ch.title }</div>
                            </div>
                        }
                    }) }
                    <div style={bottom_spacer.clone()}></div>
                </div>

                <div class="tp__epg__programs">
                    <div
                        class="tp__epg__timeline-shell"
                        ref={timeline_ref.clone()}
                        onmousedown={handle_timeline_mouse_down}
                        onwheel={handle_timeline_wheel}
                        ontouchstart={handle_timeline_touch_start}
                        ontouchmove={handle_timeline_touch_move}
                        ontouchend={handle_timeline_touch_end.clone()}
                        ontouchcancel={handle_timeline_touch_end}
                    >
                        { (*timeline_html).clone() }
                    </div>
                    <div
                        class="tp__epg__program-grid"
                        ref={grid_ref.clone()}
                        style={content_style}
                        onmousedown={handle_programs_mouse_down}
                    >
                        <div style={top_spacer}></div>
                        { for visible.iter().map(|&idx| {
                            let ch = &model.channels[idx];
                            html! {
                                <EpgRow
                                    key={ch.key}
                                    channel={Rc::clone(ch)}
                                    from_min={viewport.from_min}
                                    to_min={viewport.to_min}
                                    pixels_per_min={ppm}
                                    now_min={*now_min}
                                    row_style={row_style.clone()}
                                    can_record={can_record}
                                    on_record={handle_record.clone()}
                                />
                            }
                        }) }
                        <div style={bottom_spacer}></div>
                        <div ref={now_line_ref.clone()} class="tp__epg__now-line"></div>
                    </div>
                </div>
                </>
            }
        }
    };

    // Group list of a target source; outside the scroll container, so the viewport math of the
    // grid is unaffected. Its width change only needs a viewport recompute.
    let group_panel = if is_hosted_epg && !groups.is_empty() {
        let recompute = recompute.clone();
        html! {
            <HorizontalShrinkPanel
                class="tp__epg__group-panel"
                shrink_width="4.5rem"
                default_width="14rem"
                max_width="24rem"
                storage_key="tp-epg-group-panel-width"
                start_shrunk={is_mobile_viewport()}
                on_width_change={Callback::from(move |_| recompute())}
            >
                <EpgGroupList
                    title={translate.t("LABEL.EPG_GROUPS")}
                    groups={(*groups).clone()}
                    selected={(*selected_group).as_ref().map(|(_, name)| Arc::clone(name))}
                    disabled={!*groups_ready}
                    on_select={handle_select_group}
                />
            </HorizontalShrinkPanel>
        }
    } else {
        html! {}
    };

    html! {
        <div class="tp__epg tp__list-view">
            <div class="tp__epg__header">
                <h1>{translate.t("LABEL.PLAYLIST_EPG")}</h1>
                <div class="tp__epg__header-toolbar">
                    <Search onsearch={handle_search}/>
                </div>
            </div>
            <EpgSourceSelector on_select={handle_select_source} />
            <div class="tp__epg__main">
                { group_panel }
                <div class="tp__epg__body" ref={container_ref}>
                    { body }
                </div>
            </div>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::{epg_channel_filter, pending_program_source_id, EpgGridModel};
    use shared::{
        model::{EpgChannel, EpgChannelFilter, EpgGridRow, EpgTv, SearchRequest},
        utils::Internable,
    };

    #[test]
    fn epg_channel_filter_maps_search() {
        assert_eq!(epg_channel_filter(&SearchRequest::Clear), None);
        assert_eq!(epg_channel_filter(&SearchRequest::Text("  ".into(), None)), None);
        assert_eq!(
            epg_channel_filter(&SearchRequest::Text("zdf".into(), None)),
            Some(EpgChannelFilter::Text("zdf".into()))
        );
        assert_eq!(
            epg_channel_filter(&SearchRequest::Regexp("^ZDF".into(), None)),
            Some(EpgChannelFilter::Regexp("^ZDF".into()))
        );
    }

    #[test]
    fn pending_program_source_id_prefers_virtual_id() {
        let rows = vec![EpgGridRow {
            virtual_id: 42,
            name: "ZDF".intern(),
            logo: "".intern(),
            epg_channel_id: "123".intern(),
            programmes: vec![],
        }];
        let grid = EpgGridModel::from_grid_rows(&rows, |_| 0);
        assert_eq!(pending_program_source_id(&grid.channels[0]), "42");
    }

    #[test]
    fn pending_program_source_id_falls_back_to_epg_id() {
        let full = EpgGridModel::from_epg_tv(&EpgTv::new(vec![EpgChannel::new("123".intern())]), |_| 0);
        assert_eq!(pending_program_source_id(&full.channels[0]), "123");
    }
}
