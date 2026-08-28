use crate::{
    app::{
        components::{
            recording::{
                ensure_recording_available, epg_programme_to_prefill, target_name_for_id, EpgProgrammePrefillInput,
                PaddingBounds, RecordingForm,
            },
            EpgSourceSelector, IconButton, NoContent, Search,
        },
        context::ConfigContext,
    },
    hooks::use_service_context,
    i18n::use_translation,
    model::{BusyStatus, DialogAction, DialogActions, DialogResult, EventMessage},
    services::{CreateRecordingTaskRequest, DialogService, RecordingService, RecordingSourceInput},
    utils::set_timeout,
};
use chrono::{Datelike, Local, TimeZone, Utc};
use gloo_timers::callback::{Interval, Timeout};
use shared::{
    concat_string,
    model::{EpgTv, Permission, PlaylistEpgRequest, SearchRequest, XtreamCluster},
};
use std::{cell::RefCell, rc::Rc};
use wasm_bindgen::{prelude::Closure, JsCast};
use web_sys::{window, HtmlElement, MouseEvent, TouchEvent, WheelEvent};
use yew::{
    classes, component, html, platform::spawn_local, use_context, use_effect_with, use_memo, use_mut_ref, use_node_ref,
    use_state, Callback, Html,
};

const TIME_BLOCK_MINS: i64 = 30;
const DEFAULT_PIXELS_PER_MIN: f64 = 7.0; // 210px / 30min
const MIN_PIXELS_PER_MIN: f64 = 2.0;
const MAX_PIXELS_PER_MIN: f64 = 28.0;
const WHEEL_ZOOM_FACTOR_IN: f64 = 1.1;
const WHEEL_ZOOM_FACTOR_OUT: f64 = 1.0 / WHEEL_ZOOM_FACTOR_IN;
const ZOOM_EQUALITY_TOLERANCE: f64 = 0.01;

#[derive(Clone, Copy)]
struct TimelineZoom(f64);

impl TimelineZoom {
    fn new(pixels_per_min: f64) -> Self { Self(pixels_per_min.clamp(MIN_PIXELS_PER_MIN, MAX_PIXELS_PER_MIN)) }

    fn value(self) -> f64 { self.0 }
}

impl PartialEq for TimelineZoom {
    fn eq(&self, other: &Self) -> bool { (self.0 - other.0).abs() < ZOOM_EQUALITY_TOLERANCE }
}

fn compute_zoomed_scroll_left(
    scroll_left: f64,
    anchor_x: f64,
    current_pixels_per_min: f64,
    next_pixels_per_min: f64,
) -> f64 {
    if current_pixels_per_min <= 0.0 || next_pixels_per_min <= 0.0 {
        return scroll_left.max(0.0);
    }
    let ratio = next_pixels_per_min / current_pixels_per_min;
    (scroll_left + anchor_x * (ratio - 1.0)).max(0.0)
}

fn compute_panned_scroll(start_scroll: i32, start_pointer: i32, current_pointer: i32) -> i32 {
    start_scroll - (current_pointer - start_pointer)
}

fn get_pos(secs: i64, start_mins: i64, pixels_per_min: f64) -> i64 {
    let mins = secs / 60;
    let rel_mins = mins - start_mins;
    (rel_mins as f64 * pixels_per_min).round() as i64
}

#[cfg(test)]
mod tests {
    use super::{compute_panned_scroll, compute_zoomed_scroll_left, TimelineZoom};

    #[test]
    fn timeline_zoom_new_clamps_bounds() {
        assert_eq!(TimelineZoom::new(0.5).value(), 2.0);
        assert_eq!(TimelineZoom::new(999.0).value(), 28.0);
        assert_eq!(TimelineZoom::new(7.0).value(), 7.0);
    }

    #[test]
    fn compute_zoomed_scroll_left_keeps_anchor_stable() {
        // Zoom in 2x with anchor at 80px, scroll at 120px
        // After zoom, anchor pixel moves to 160px.
        // New scroll = 120 + 80*(2-1) = 200, so anchor stays at same viewport pos.
        let next = compute_zoomed_scroll_left(120.0, 80.0, 4.0, 8.0);
        assert_eq!(next, 200.0);
    }

    #[test]
    fn compute_zoomed_scroll_left_zero_anchor_does_not_shift() {
        let next = compute_zoomed_scroll_left(100.0, 0.0, 4.0, 8.0);
        assert_eq!(next, 100.0);
    }

    #[test]
    fn compute_zoomed_scroll_left_zoom_out() {
        // Zoom out 2x with anchor at 200px, scroll at 300px
        // After zoom, anchor pixel moves to 100px.
        // New scroll = 300 + 200*(0.5-1) = 200
        let next = compute_zoomed_scroll_left(300.0, 200.0, 8.0, 4.0);
        assert_eq!(next, 200.0);
    }

    #[test]
    fn compute_panned_scroll_moves_in_drag_direction() {
        assert_eq!(compute_panned_scroll(300, 100, 140), 260);
        assert_eq!(compute_panned_scroll(300, 100, 60), 340);
    }
}

type OnScrollHandle = Rc<RefCell<Option<Closure<dyn FnMut(web_sys::Event)>>>>;
type MouseMoveHandle = Rc<RefCell<Option<Closure<dyn FnMut(MouseEvent)>>>>;
type MouseUpHandle = Rc<RefCell<Option<Closure<dyn FnMut(MouseEvent)>>>>;
type RafClosure = Rc<RefCell<Option<Closure<dyn FnMut(f64)>>>>;

fn register_mouse_pan(
    is_active: bool,
    on_move: Box<dyn FnMut(MouseEvent)>,
    on_up: Box<dyn FnMut(MouseEvent)>,
) -> (Option<MouseMoveHandle>, Option<MouseUpHandle>) {
    if !is_active {
        return (None, None);
    }
    let Some(win) = window() else { return (None, None) };
    let move_handle: MouseMoveHandle = Rc::new(RefCell::new(None));
    let up_handle: MouseUpHandle = Rc::new(RefCell::new(None));

    let mouse_move = Closure::wrap(on_move);
    let _ = win.add_event_listener_with_callback("mousemove", mouse_move.as_ref().unchecked_ref());
    *move_handle.borrow_mut() = Some(mouse_move);

    let mouse_up = Closure::wrap(on_up);
    let _ = win.add_event_listener_with_callback("mouseup", mouse_up.as_ref().unchecked_ref());
    *up_handle.borrow_mut() = Some(mouse_up);

    (Some(move_handle), Some(up_handle))
}

fn unregister_mouse_pan(move_handle: MouseMoveHandle, up_handle: MouseUpHandle) {
    if let Some(win) = window() {
        if let Some(mouse_move) = move_handle.borrow_mut().take() {
            let _ = win.remove_event_listener_with_callback("mousemove", mouse_move.as_ref().unchecked_ref());
        }
        if let Some(mouse_up) = up_handle.borrow_mut().take() {
            let _ = win.remove_event_listener_with_callback("mouseup", mouse_up.as_ref().unchecked_ref());
        }
    }
}

#[derive(Clone, Copy)]
struct TimelinePinchState {
    distance: f64,
    anchor_x: f64,
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

#[derive(Clone)]
struct PendingProgram {
    channel_id: String,
    channel_name: Option<String>,
    title: String,
    start: i64,
    stop: i64,
}

fn update_now_line(
    container_ref: &yew::NodeRef,
    now_line_ref: &yew::NodeRef,
    epg_window: &Option<(i64, i64)>,
    pixels_per_min: f64,
    recenter: bool,
) {
    if let Some((start, stop)) = epg_window {
        if let (Some(div), Some(now_line)) = (container_ref.cast::<HtmlElement>(), now_line_ref.cast::<HtmlElement>()) {
            let now = Utc::now().timestamp();
            if now >= *start && now <= *stop {
                let start_window_secs = (*start / (TIME_BLOCK_MINS * 60)) * (TIME_BLOCK_MINS * 60);
                let start_window = (start_window_secs / 60).max(0);
                let now_line_pos = get_pos(now, start_window, pixels_per_min);
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
    }
}

#[component]
pub fn EpgView() -> Html {
    let services = use_service_context();
    let config_ctx = use_context::<ConfigContext>().expect("ConfigContext not found");
    let dialog = use_context::<DialogService>().expect("Dialog service not found");
    let translate = use_translation();
    let epg = use_state::<Option<EpgTv>, _>(|| None);
    let container_ref = use_node_ref();
    let now_line_ref = use_node_ref();
    let timeline_ref = use_node_ref();
    let pixels_per_min = use_state(|| TimelineZoom::new(DEFAULT_PIXELS_PER_MIN));
    // pending_scroll_left is mutated via use_mut_ref (no re-render).
    // It is read inside a use_effect_with triggered by pixels_per_min changes.
    let pending_scroll_left = use_mut_ref(|| None::<i32>);
    let pending_zoom = use_mut_ref(|| None::<(f64, f64)>);
    let raf_id = use_mut_ref(|| None::<i32>);
    let raf_closure = RafClosure::new(RefCell::new(None));
    let epg_request_token = use_mut_ref(|| 0u32);
    let pinch_state = use_mut_ref(|| None::<TimelinePinchState>);
    let program_pan_state = use_mut_ref(|| None::<ProgramPanState>);
    let timeline_pan_state = use_mut_ref(|| None::<TimelinePanState>);
    let is_program_panning = use_state(|| false);
    let is_timeline_panning = use_state(|| false);
    let selected_epg_source = use_state(|| None::<PlaylistEpgRequest>);
    let can_write_recordings = services.auth.has_permission(Permission::RecordingWrite);
    let is_admin_role = services.auth.is_admin();
    let recording_padding = {
        let rec = config_ctx
            .config
            .as_ref()
            .and_then(|cfg| cfg.config.video.as_ref())
            .and_then(|video| video.recording.as_ref());
        Rc::new(PaddingBounds {
            default_pre_roll_secs: rec.and_then(|c| c.default_pre_roll_secs).unwrap_or(0),
            max_pre_roll_secs: rec.map_or(900, |c| c.max_pre_roll_secs),
            default_post_roll_secs: rec.and_then(|c| c.default_post_roll_secs).unwrap_or(0),
            max_post_roll_secs: rec.map_or(1800, |c| c.max_post_roll_secs),
        })
    };

    // State to keep track of visible channel range
    let visible_range = use_state(|| (0, 20)); // (start_index, end_index)
    let search_filter = use_state::<SearchRequest, _>(|| SearchRequest::Clear);
    let is_hosted_epg = matches!(*selected_epg_source, Some(PlaylistEpgRequest::Target(_)));

    let handle_search = {
        let search_filter = search_filter.clone();
        let visible_range = visible_range.clone();
        let container_ref = container_ref.clone();
        Callback::from(move |req: SearchRequest| {
            search_filter.set(req);
            visible_range.set((0, 20));
            if let Some(el) = container_ref.cast::<HtmlElement>() {
                el.set_scroll_top(0);
            }
        })
    };

    let handle_select_source = {
        let service_ctx = services.clone();
        let epg_set = epg.clone();
        let search_filter = search_filter.clone();
        let visible_range = visible_range.clone();
        let container_ref = container_ref.clone();
        let pixels_per_min = pixels_per_min.clone();
        let pending_scroll_left = pending_scroll_left.clone();
        let pending_zoom = pending_zoom.clone();
        let raf_id = raf_id.clone();
        let raf_closure = raf_closure.clone();
        let epg_request_token = epg_request_token.clone();
        let selected_epg_source = selected_epg_source.clone();
        Callback::from(move |req: PlaylistEpgRequest| {
            selected_epg_source.set(Some(req.clone()));
            epg_set.set(None);
            search_filter.set(SearchRequest::Clear);
            visible_range.set((0, 20));
            pixels_per_min.set(TimelineZoom::new(DEFAULT_PIXELS_PER_MIN));
            *pending_scroll_left.borrow_mut() = Some(0);
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
            let token = {
                let mut t = epg_request_token.borrow_mut();
                *t = t.wrapping_add(1);
                *t
            };
            let service_ctx = service_ctx.clone();
            let epg_set = epg_set.clone();
            let epg_request_token = epg_request_token.clone();
            service_ctx.event.broadcast(EventMessage::Busy(BusyStatus::Show));
            spawn_local(async move {
                let playlist_epg = service_ctx.playlist.get_playlist_epg(req).await;
                if token != *epg_request_token.borrow() {
                    return;
                }
                set_timeout(
                    move || {
                        service_ctx.event.broadcast(EventMessage::Busy(BusyStatus::Hide));
                        epg_set.set(playlist_epg);
                    },
                    16,
                );
            });
        })
    };

    let epg_window = (*epg).as_ref().map(|tv| (tv.start, tv.stop));
    let timeline_zoom = pixels_per_min.value();

    let timeline_layout = use_memo((epg_window, timeline_zoom), |(epg_window, timeline_zoom)| {
        let Some((start, stop)) = *epg_window else {
            return (0i64, 0i64, String::new());
        };
        let start_window_secs = (start / (TIME_BLOCK_MINS * 60)) * (TIME_BLOCK_MINS * 60);
        let start_window = (start_window_secs / 60).max(0);
        let end_window = (stop / 60).max(0);
        let window_duration = (end_window - start_window).max(0);
        let num_blocks = (window_duration + TIME_BLOCK_MINS - 1) / TIME_BLOCK_MINS;
        let timeline_content_width = (num_blocks as f64 * (timeline_zoom * TIME_BLOCK_MINS as f64)).round() as i64;
        let timeline_content_style = format!(
            "width:{timeline_content_width}px; min-width:{timeline_content_width}px; max-width:{timeline_content_width}px"
        );
        (start_window, num_blocks, timeline_content_style)
    });

    // Memoized timeline — only rebuilt when EPG data changes, not on every scroll
    let timeline_html = use_memo(
        (epg_window, timeline_zoom, timeline_layout.clone()),
        |(epg_window, timeline_zoom, timeline_layout)| {
            let (start_window, num_blocks, _) = **timeline_layout;
            let Some((_, _)) = *epg_window else { return html! {} };
            if num_blocks == 0 {
                return html! {};
            }
            let block_width = timeline_zoom * TIME_BLOCK_MINS as f64;
            let block_style = format!("width:{block_width}px; min-width:{block_width}px; max-width:{block_width}px");
            html! {
                <div class="tp__epg__timeline">
                    { for (0..num_blocks).map(|i| {
                        let block_start = start_window + i * TIME_BLOCK_MINS;
                        let block_secs = block_start.saturating_mul(60);
                        if let Some(start_time_utc) = Utc.timestamp_opt(block_secs, 0).single() {
                            let start_time_local = start_time_utc.with_timezone(&Local);
                            let hour_min = start_time_local.format("%H:%M").to_string();
                            let day_month = format!("{:02}.{:02}", start_time_local.day(), start_time_local.month());
                            html! {
                                <div class="tp__epg__timeline-block" style={block_style.clone()}>
                                    <div class="tp__epg__timeline-block-time">{ hour_min }</div>
                                    <div class="tp__epg__timeline-block-date">{ day_month }</div>
                                </div>
                            }
                        } else {
                            html!{ <div class="tp__epg__timeline-block" style={block_style.clone()}></div> }
                        }
                    }) }
                </div>
            }
        },
    );

    {
        let container_ref = container_ref.clone();
        let pending_scroll_left = pending_scroll_left.clone();
        use_effect_with(pixels_per_min.value(), move |_| {
            if let Some(scroll_left) = pending_scroll_left.borrow_mut().take() {
                if let Some(container) = container_ref.cast::<HtmlElement>() {
                    container.set_scroll_left(scroll_left);
                }
            }
            || ()
        });
    }

    {
        let raf_id = raf_id.clone();
        let raf_closure = raf_closure.clone();
        use_effect_with((), move |()| {
            move || {
                if let Some(id) = raf_id.borrow_mut().take() {
                    if let Some(win) = window() {
                        let _ = win.cancel_animation_frame(id);
                    }
                }
                *raf_closure.borrow_mut() = None;
            }
        });
    }

    {
        let container_ref = container_ref.clone();
        let now_line_ref = now_line_ref.clone();
        use_effect_with((epg_window, pixels_per_min.value()), move |(epg_window, pixels_per_min)| {
            let epg_window_clone = *epg_window;
            let pixels_per_min = *pixels_per_min;
            let update_position = Rc::new(move |epg_window: &Option<(i64, i64)>| {
                update_now_line(&container_ref, &now_line_ref, epg_window, pixels_per_min, false);
            });

            let calculate_pos = update_position.clone();
            let interval = Interval::new(60_000, move || {
                calculate_pos(&epg_window_clone);
            });
            update_position(epg_window);
            || drop(interval)
        });
    }

    {
        let container_ref = container_ref.clone();
        let now_line_ref = now_line_ref.clone();
        let pixels_per_min = pixels_per_min.clone();
        use_effect_with(epg_window, move |epg_window| {
            update_now_line(&container_ref, &now_line_ref, epg_window, pixels_per_min.value(), true);
            || ()
        });
    }

    let apply_timeline_zoom = {
        let pending_zoom = pending_zoom.clone();
        let raf_id = raf_id.clone();
        let raf_closure = raf_closure.clone();
        let pixels_per_min = pixels_per_min.clone();
        let container_ref = container_ref.clone();
        let pending_scroll_left = pending_scroll_left.clone();
        Callback::from(move |(next_pixels_per_min, anchor_x): (f64, f64)| {
            let next_pixels_per_min = TimelineZoom::new(next_pixels_per_min).value();
            *pending_zoom.borrow_mut() = Some((next_pixels_per_min, anchor_x));

            // If RAF already scheduled, it will pick up the latest pending_zoom
            if raf_id.borrow().is_some() {
                return;
            }

            let Some(win) = window() else { return };

            let pending_zoom_r = pending_zoom.clone();
            let raf_id_r = raf_id.clone();
            let pixels_per_min_r = pixels_per_min.clone();
            let container_ref_r = container_ref.clone();
            let pending_scroll_left_r = pending_scroll_left.clone();

            let closure = Closure::wrap(Box::new(move |_ts: f64| {
                *raf_id_r.borrow_mut() = None;
                let Some((next_ppm, anchor_x)) = pending_zoom_r.borrow_mut().take() else {
                    return;
                };
                let current_ppm = pixels_per_min_r.0;
                if (next_ppm - current_ppm).abs() < ZOOM_EQUALITY_TOLERANCE {
                    return;
                }
                let scroll_left = container_ref_r.cast::<HtmlElement>().map_or(0.0, |c| f64::from(c.scroll_left()));
                let next_scroll = compute_zoomed_scroll_left(scroll_left, anchor_x.max(0.0), current_ppm, next_ppm);
                *pending_scroll_left_r.borrow_mut() = Some(next_scroll.round() as i32);
                pixels_per_min_r.set(TimelineZoom::new(next_ppm));
            }) as Box<dyn FnMut(f64)>);

            if let Ok(id) = win.request_animation_frame(closure.as_ref().unchecked_ref()) {
                *raf_id.borrow_mut() = Some(id);
                *raf_closure.borrow_mut() = Some(closure);
            }
        })
    };

    let handle_timeline_wheel = {
        let timeline_ref = timeline_ref.clone();
        let apply_timeline_zoom = apply_timeline_zoom.clone();
        let pixels_per_min = pixels_per_min.clone();
        Callback::from(move |e: WheelEvent| {
            // The time header is the only place wheel zooms; elsewhere we let
            // the container scroll so the EPG can stream-load more rows.
            e.prevent_default();
            e.stop_propagation();
            if let Some(timeline) = timeline_ref.cast::<HtmlElement>() {
                let rect = timeline.get_bounding_client_rect();
                let anchor_x = f64::from(e.client_x()) - rect.left();
                let next_pixels_per_min = if e.delta_y() < 0.0 {
                    pixels_per_min.value() * WHEEL_ZOOM_FACTOR_IN
                } else {
                    pixels_per_min.value() * WHEEL_ZOOM_FACTOR_OUT
                };
                apply_timeline_zoom.emit((next_pixels_per_min, anchor_x));
            }
        })
    };

    let handle_timeline_touch_start = {
        let timeline_ref = timeline_ref.clone();
        let container_ref = container_ref.clone();
        let pinch_state = pinch_state.clone();
        let timeline_pan_state = timeline_pan_state.clone();
        let is_timeline_panning = is_timeline_panning.clone();
        let pixels_per_min = pixels_per_min.clone();
        Callback::from(move |e: TouchEvent| {
            let touch_count = e.touches().length();
            if touch_count == 2 {
                let Some(first) = e.touches().item(0) else {
                    return;
                };
                let Some(second) = e.touches().item(1) else {
                    return;
                };
                let Some(timeline) = timeline_ref.cast::<HtmlElement>() else {
                    return;
                };

                e.prevent_default();
                e.stop_propagation();

                *timeline_pan_state.borrow_mut() = None;
                is_timeline_panning.set(false);

                let rect = timeline.get_bounding_client_rect();
                let dx = f64::from(second.client_x() - first.client_x());
                let dy = f64::from(second.client_y() - first.client_y());
                let distance = (dx * dx + dy * dy).sqrt();
                let midpoint_x = f64::midpoint(f64::from(first.client_x()), f64::from(second.client_x()));

                *pinch_state.borrow_mut() = Some(TimelinePinchState {
                    distance,
                    anchor_x: midpoint_x - rect.left(),
                    pixels_per_min: pixels_per_min.value(),
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
                is_timeline_panning.set(true);
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

                apply_timeline_zoom.emit((initial.pixels_per_min * (distance / initial.distance), initial.anchor_x));
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
        let is_timeline_panning = is_timeline_panning.clone();
        Callback::from(move |_e: TouchEvent| {
            *pinch_state.borrow_mut() = None;
            *timeline_pan_state.borrow_mut() = None;
            is_timeline_panning.set(false);
        })
    };

    let handle_timeline_mouse_down = {
        let container_ref = container_ref.clone();
        let timeline_pan_state = timeline_pan_state.clone();
        let is_timeline_panning = is_timeline_panning.clone();
        Callback::from(move |e: MouseEvent| {
            if e.button() != 0 {
                return;
            }
            let Some(container) = container_ref.cast::<HtmlElement>() else {
                return;
            };
            e.prevent_default();
            e.stop_propagation();
            *timeline_pan_state.borrow_mut() =
                Some(TimelinePanState { pointer_x: e.client_x(), scroll_left: container.scroll_left() });
            is_timeline_panning.set(true);
        })
    };

    {
        let container_ref = container_ref.clone();
        let timeline_pan_state = timeline_pan_state.clone();
        let is_timeline_panning_handle = is_timeline_panning.clone();
        use_effect_with(*is_timeline_panning, move |is_timeline_panning| {
            let container_ref_m = container_ref.clone();
            let timeline_pan_state_m = timeline_pan_state.clone();
            let timeline_pan_state_u = timeline_pan_state.clone();
            let is_timeline_panning_u = is_timeline_panning_handle.clone();
            let (move_handle, up_handle) = register_mouse_pan(
                *is_timeline_panning,
                Box::new(move |e: MouseEvent| {
                    let Some(pan_state) = *timeline_pan_state_m.borrow() else { return };
                    let Some(container) = container_ref_m.cast::<HtmlElement>() else { return };
                    e.prevent_default();
                    container.set_scroll_left(compute_panned_scroll(
                        pan_state.scroll_left,
                        pan_state.pointer_x,
                        e.client_x(),
                    ));
                }),
                Box::new(move |_e: MouseEvent| {
                    *timeline_pan_state_u.borrow_mut() = None;
                    is_timeline_panning_u.set(false);
                }),
            );
            move || {
                if let (Some(mh), Some(uh)) = (move_handle, up_handle) {
                    unregister_mouse_pan(mh, uh);
                }
            }
        });
    }

    let handle_programs_mouse_down = {
        let container_ref = container_ref.clone();
        let program_pan_state = program_pan_state.clone();
        let is_program_panning = is_program_panning.clone();
        Callback::from(move |e: MouseEvent| {
            if e.button() != 0 {
                return;
            }
            let Some(container) = container_ref.cast::<HtmlElement>() else {
                return;
            };
            e.prevent_default();
            e.stop_propagation();
            *program_pan_state.borrow_mut() = Some(ProgramPanState {
                pointer_x: e.client_x(),
                pointer_y: e.client_y(),
                scroll_left: container.scroll_left(),
                scroll_top: container.scroll_top(),
            });
            is_program_panning.set(true);
        })
    };

    {
        let container_ref = container_ref.clone();
        let program_pan_state = program_pan_state.clone();
        let is_program_panning_handle = is_program_panning.clone();
        use_effect_with(*is_program_panning, move |is_program_panning| {
            let container_ref_m = container_ref.clone();
            let program_pan_state_m = program_pan_state.clone();
            let program_pan_state_u = program_pan_state.clone();
            let is_program_panning_u = is_program_panning_handle.clone();
            let (move_handle, up_handle) = register_mouse_pan(
                *is_program_panning,
                Box::new(move |e: MouseEvent| {
                    let Some(pan_state) = *program_pan_state_m.borrow() else { return };
                    let Some(container) = container_ref_m.cast::<HtmlElement>() else { return };
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
                }),
                Box::new(move |_e: MouseEvent| {
                    *program_pan_state_u.borrow_mut() = None;
                    is_program_panning_u.set(false);
                }),
            );
            move || {
                if let (Some(mh), Some(uh)) = (move_handle, up_handle) {
                    unregister_mouse_pan(mh, uh);
                }
            }
        });
    }

    let handle_programs_touch_start = {
        let container_ref = container_ref.clone();
        let program_pan_state = program_pan_state.clone();
        let is_program_panning = is_program_panning.clone();
        Callback::from(move |e: TouchEvent| {
            if e.touches().length() != 1 {
                *program_pan_state.borrow_mut() = None;
                is_program_panning.set(false);
                return;
            }
            let Some(container) = container_ref.cast::<HtmlElement>() else {
                return;
            };
            let Some(touch) = e.touches().item(0) else {
                return;
            };
            e.prevent_default();
            e.stop_propagation();
            *program_pan_state.borrow_mut() = Some(ProgramPanState {
                pointer_x: touch.client_x(),
                pointer_y: touch.client_y(),
                scroll_left: container.scroll_left(),
                scroll_top: container.scroll_top(),
            });
            is_program_panning.set(true);
        })
    };

    let handle_programs_touch_move = {
        let container_ref = container_ref.clone();
        let program_pan_state = program_pan_state.clone();
        Callback::from(move |e: TouchEvent| {
            if e.touches().length() != 1 {
                return;
            }
            let Some(pan_state) = *program_pan_state.borrow() else {
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
            container.set_scroll_top(compute_panned_scroll(
                pan_state.scroll_top,
                pan_state.pointer_y,
                touch.client_y(),
            ));
        })
    };

    let handle_programs_touch_end = {
        let program_pan_state = program_pan_state.clone();
        let is_program_panning = is_program_panning.clone();
        Callback::from(move |_e: TouchEvent| {
            *program_pan_state.borrow_mut() = None;
            is_program_panning.set(false);
        })
    };

    let row_height = use_memo((), move |()| {
        let row_height = window()
            .and_then(|win| {
                let root = win.document()?.document_element()?;
                win.get_computed_style(&root).ok().flatten()
            })
            .and_then(|style| style.get_property_value("--epg-row-height").ok())
            .unwrap_or_default();

        row_height.trim_end_matches("px").parse::<usize>().unwrap_or(60).max(1)
    });

    // Add scroll listener to calculate visible channels
    {
        let container_ref = container_ref.clone();
        let visible_range = visible_range.clone();
        let channel_row_height = *row_height;
        use_effect_with((), move |()| {
            let debounce_handle: Rc<RefCell<Option<Timeout>>> = Rc::new(RefCell::new(None));
            let onscroll_handle: OnScrollHandle = Rc::new(RefCell::new(None));
            let cleanup_container_ref = container_ref.clone();
            if let Some(div) = container_ref.cast::<HtmlElement>() {
                let visible_range = visible_range.clone();
                // Store debounce timer in Rc<RefCell>
                let debounce_handle_clone = debounce_handle.clone();
                let onscroll_handle_clone = onscroll_handle.clone();
                let onscroll = Closure::wrap(Box::new(move |_event: web_sys::Event| {
                    // Cancel previous scheduled update
                    if let Some(prev) = debounce_handle_clone.borrow_mut().take() {
                        prev.cancel();
                    }
                    // Schedule a new update after X ms (debounce)
                    let container_ref = container_ref.clone();
                    let vr = visible_range.clone();
                    let handle = Timeout::new(50, move || {
                        if let Some(div) = container_ref.cast::<HtmlElement>() {
                            let scroll_top = div.scroll_top();
                            let client_height = div.client_height();
                            let start_index = (scroll_top / (channel_row_height as i32) - 10).max(0) as usize;
                            let end_index =
                                ((scroll_top + client_height) / (channel_row_height as i32) + 10).max(0) as usize;
                            // Only trigger re-render when visible range actually changed
                            if *vr != (start_index, end_index) {
                                vr.set((start_index, end_index));
                            }
                        }
                    });

                    *debounce_handle_clone.borrow_mut() = Some(handle);
                }) as Box<dyn FnMut(_)>);
                if let Err(err) = div.add_event_listener_with_callback("scroll", onscroll.as_ref().unchecked_ref()) {
                    log::error!("Failed to register EPG scroll listener: {err:?}");
                }
                *onscroll_handle_clone.borrow_mut() = Some(onscroll);
            }
            move || {
                if let Some(prev) = debounce_handle.borrow_mut().take() {
                    prev.cancel();
                }
                if let Some(onscroll) = onscroll_handle.borrow_mut().take() {
                    // Detach before dropping the closure so a live element cannot invoke a destroyed callback
                    if let Some(div) = cleanup_container_ref.cast::<HtmlElement>() {
                        let _ = div.remove_event_listener_with_callback("scroll", onscroll.as_ref().unchecked_ref());
                    }
                    drop(onscroll);
                }
            }
        });
    }

    let handle_record_click = {
        let selected_epg_source = selected_epg_source.clone();
        let recording_padding = recording_padding.clone();
        let dialog = dialog.clone();
        let services = services.clone();
        let translate = translate.clone();
        let config = config_ctx.config.clone();
        Callback::from(move |(program, _event): (PendingProgram, MouseEvent)| {
            let Some(PlaylistEpgRequest::Target(target_id)) = (*selected_epg_source).clone() else {
                services.toastr.error(translate.t("MESSAGES.RECORDING.NO_TARGET"));
                return;
            };
            let Some(target_name) =
                config.as_ref().and_then(|app_config| target_name_for_id(&app_config.sources, target_id, None))
            else {
                services.toastr.error(translate.t("MESSAGES.RECORDING.NO_TARGET"));
                return;
            };
            let dialog = dialog.clone();
            let services = services.clone();
            let translate = translate.clone();
            let padding: PaddingBounds = (*recording_padding).clone();
            spawn_local(async move {
                if !ensure_recording_available(&services, &translate).await {
                    return;
                }
                let source = RecordingSourceInput {
                    target_id: target_name,
                    virtual_id: program.channel_id.clone(),
                    cluster: XtreamCluster::Live,
                    input_name: String::new(),
                };
                let mut prefill = epg_programme_to_prefill(EpgProgrammePrefillInput {
                    source,
                    channel_id: Some(program.channel_id.clone()),
                    channel_name: program.channel_name.clone(),
                    programme_title: program.title,
                    programme_start: program.start,
                    programme_end: program.stop,
                    padding: padding.clone(),
                    episode: None,
                });
                if let Some(name) = program.channel_name.clone() {
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
                        has_recording_write={can_write_recordings}
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
                    Ok(_) => services.toastr.success(translate.t("MESSAGES.RECORDING.QUEUED")),
                    Err(err) => services.toastr.error(err.to_string()),
                }
            });
        })
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
            <div class="tp__epg__body" ref={container_ref}>
                {
                    if epg.is_none() {
                        html! { <NoContent text={translate.t("MESSAGES.EPG.SELECT_AN_EPG_TO_VIEW_CONTENT")} hint={translate.t("MESSAGES.EPG.SELECT_AN_EPG_HINT")}/> }
                   } else {
                        let tv = epg.as_ref().unwrap();
                        let now = Utc::now().timestamp();
                        let (start_window, _num_blocks, timeline_content_style) = &*timeline_layout;

                        let filtered_channels: Vec<_> = tv.channels.iter()
                            .filter(|ch| match &*search_filter {
                                SearchRequest::Clear => true,
                                SearchRequest::Text(pattern, _) => {
                                    let lc = pattern.to_lowercase();
                                    ch.title.as_ref().is_some_and(|t| t.to_lowercase().contains(&lc))
                                }
                                SearchRequest::Regexp(pattern, _) => {
                                    ch.title.as_deref().is_some_and(|t| {
                                        shared::model::REGEX_CACHE
                                            .get_or_compile(pattern)
                                            .is_ok_and(|re| re.is_match(t))
                                    })
                                }
                            })
                            .collect();

                        let (start_index, end_index) = *visible_range;
                        let total_channels = filtered_channels.len();
                        let channel_row_height = *row_height;

                        html! {
                        <>
                        <div class="tp__epg__channels">
                            <div class="tp__epg__channels-header"></div>
                            <div style={format!("height:{}px", start_index * channel_row_height)}></div>
                            { for filtered_channels.iter().enumerate().skip(start_index).take(end_index - start_index).map(|(i, ch)| {
                                html! {
                                    <div key={i} class="tp__epg__channel" title={concat_string!(&ch.title.as_ref().map(ToString::to_string).unwrap_or_default(), " (",  &ch.id, ")")}
                                         style={format!("max-height:{channel_row_height}px;min-height:{channel_row_height}px;height:{channel_row_height}px")}>
                                        <div class="tp__epg__channel-icon">
                                            { if let Some(icon) = &ch.icon {
                                                html! { <img src={icon.to_string()} alt={ch.title.as_ref().map(ToString::to_string).unwrap_or_default()} /> }
                                              } else { html!{} }
                                            }
                                        </div>
                                        <div class="tp__epg__channel-title">
                                            { ch.title.as_ref().map(ToString::to_string).unwrap_or_default() }
                                        </div>
                                    </div>
                                }
                              })
                            }
                            <div style={format!("height:{}px", (total_channels.saturating_sub(end_index)) * channel_row_height)}></div>
                        </div>

                        <div
                            class={classes!("tp__epg__programs", (*is_program_panning).then_some("tp__epg__programs-panning"))}
                        >
                            <div
                                class={classes!("tp__epg__timeline-shell", (*is_timeline_panning).then_some("tp__epg__timeline-shell-panning"))}
                                ref={timeline_ref}
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
                                class={classes!("tp__epg__program-grid", (*is_program_panning).then_some("tp__epg__program-grid-panning"))}
                                style={timeline_content_style.clone()}
                                onmousedown={handle_programs_mouse_down}
                                ontouchstart={handle_programs_touch_start}
                                ontouchmove={handle_programs_touch_move}
                                ontouchend={handle_programs_touch_end.clone()}
                                ontouchcancel={handle_programs_touch_end}
                            >
                                <div style={format!("height:{}px", start_index * channel_row_height)}></div>
                                { for filtered_channels.iter().enumerate().skip(start_index).take(end_index - start_index).map(|(i, ch)| {
                                    let row_style = format!(
                                        "max-height:{channel_row_height}px;min-height:{channel_row_height}px;height:{channel_row_height}px;{timeline_content_style}"
                                    );
                                    html! {
                                      <div key={i} class="tp__epg__channel-programs" style={row_style}>
                                        { for ch.programmes.iter().map(|p| {
                                            let is_active = now >= p.start && now < p.stop;
                                            let is_past = now >= p.stop;
                                            let left = get_pos(p.start, *start_window, pixels_per_min.value());
                                            let right = get_pos(p.stop, *start_window, pixels_per_min.value());
                                            let width = (right - left).max(0);

                                            if let (Some(pstart_time), Some(pend_time)) = (
                                                    Utc.timestamp_opt(p.start, 0).single(),
                                                    Utc.timestamp_opt(p.stop, 0).single()) {
                                                let pstart_time_local = pstart_time.with_timezone(&Local);
                                                let pend_time_local = pend_time.with_timezone(&Local);
                                                let pstart = pstart_time_local.format("%H:%M").to_string();
                                                let pend = pend_time_local.format("%H:%M").to_string();
                                                let program_style = format!("left:{left}px; width:{width}px; min-width:{width}px; max-width:{width}px");
                                                let pending = PendingProgram {
                                                    channel_id: ch.id.to_string(),
                                                    channel_name: ch.title.as_ref().map(ToString::to_string),
                                                    title: p.title.as_ref().map(ToString::to_string).unwrap_or_default(),
                                                    start: p.start,
                                                    stop: p.stop,
                                                };
                                                let program_record_click = {
                                                    let handle_record_click = handle_record_click.clone();
                                                    let pending = pending.clone();
                                                    Callback::from(move |(_name, event): (String, MouseEvent)| {
                                                        handle_record_click.emit((pending.clone(), event));
                                                    })
                                                };

                                                html! {
                                                <div class={classes!("tp__epg__program", if is_active { "tp__epg__program-active" } else {""})} style={program_style.clone()} title={ p.title.as_ref().map(ToString::to_string).unwrap_or_default() }>
                                                    <div class="tp__epg__program-time">{ &pstart } {"-"} { &pend }</div>
                                                    <div class="tp__epg__program-title">
                                                        { p.title.as_ref().map(ToString::to_string).unwrap_or_default() }
                                                    </div>
                                                    {
                                                        if can_write_recordings && !is_past && is_hosted_epg {
                                                            html! {
                                                                <IconButton
                                                                    name="program_record"
                                                                    icon="DVR"
                                                                    class="tp__epg__program-menu"
                                                                    onclick={program_record_click}
                                                                />
                                                            }
                                                        } else {
                                                            html! {}
                                                        }
                                                    }
                                                </div>
                                                }
                                            } else {
                                              html!{}
                                            }
                                        })}
                                      </div>
                                    }
                                  })
                                }
                                <div style={format!("height:{}px", (total_channels.saturating_sub(end_index)) * channel_row_height)}></div>
                                <div ref={now_line_ref} class="tp__epg__now-line"></div>
                            </div>
                        </div>
                        </>
                     }
                   }
                }
            </div>
        </div>
    }
}
