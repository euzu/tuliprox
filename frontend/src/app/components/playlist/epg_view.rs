use crate::{
    app::components::{EpgSourceSelector, NoContent, Search},
    hooks::use_service_context,
    i18n::use_translation,
    model::{BusyStatus, EventMessage},
    utils::set_timeout,
};
use chrono::{Datelike, Local, TimeZone, Utc};
use gloo_timers::callback::{Interval, Timeout};
use shared::{
    concat_string,
    model::{EpgTv, PlaylistEpgRequest, SearchRequest},
};
use std::{cell::RefCell, rc::Rc};
use wasm_bindgen::{prelude::Closure, JsCast};
use web_sys::{window, HtmlElement, MouseEvent, TouchEvent, WheelEvent};
use yew::{
    classes, component, html, platform::spawn_local, use_effect_with, use_memo, use_mut_ref, use_node_ref, use_state,
    Callback, Html,
};

const TIME_BLOCK_WIDTH: f64 = 210.0;
const TIME_BLOCK_MINS: i64 = 30;
const DEFAULT_PIXELS_PER_MIN: f64 = TIME_BLOCK_WIDTH / TIME_BLOCK_MINS as f64;
const MIN_PIXELS_PER_MIN: f64 = 2.0;
const MAX_PIXELS_PER_MIN: f64 = 28.0;
const WHEEL_ZOOM_FACTOR_IN: f64 = 1.1;
const WHEEL_ZOOM_FACTOR_OUT: f64 = 1.0 / WHEEL_ZOOM_FACTOR_IN;
const ZOOM_EQUALITY_TOLERANCE: f64 = 0.01;

#[derive(Clone, Copy)]
struct TimelineZoom(f64);

impl PartialEq for TimelineZoom {
    fn eq(&self, other: &Self) -> bool { (self.0 - other.0).abs() < ZOOM_EQUALITY_TOLERANCE }
}

fn clamp_timeline_zoom(pixels_per_min: f64) -> f64 { pixels_per_min.clamp(MIN_PIXELS_PER_MIN, MAX_PIXELS_PER_MIN) }

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
    use super::{clamp_timeline_zoom, compute_panned_scroll, compute_zoomed_scroll_left};

    #[test]
    fn clamp_timeline_zoom_respects_bounds() {
        assert_eq!(clamp_timeline_zoom(0.5), 2.0);
        assert_eq!(clamp_timeline_zoom(999.0), 28.0);
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
    let translate = use_translation();
    let epg = use_state::<Option<EpgTv>, _>(|| None);
    let container_ref = use_node_ref();
    let now_line_ref = use_node_ref();
    let timeline_ref = use_node_ref();
    let pixels_per_min = use_state(|| TimelineZoom(DEFAULT_PIXELS_PER_MIN));
    // pending_scroll_left is mutated via use_mut_ref (no re-render).
    // It is read inside a use_effect_with triggered by pixels_per_min changes.
    let pending_scroll_left = use_mut_ref(|| None::<i32>);
    let pinch_state = use_mut_ref(|| None::<TimelinePinchState>);
    let program_pan_state = use_mut_ref(|| None::<ProgramPanState>);
    let timeline_pan_state = use_mut_ref(|| None::<TimelinePanState>);
    let is_program_panning = use_state(|| false);
    let is_timeline_panning = use_state(|| false);

    // State to keep track of visible channel range
    let visible_range = use_state(|| (0, 20)); // (start_index, end_index)
    let search_filter = use_state::<SearchRequest, _>(|| SearchRequest::Clear);

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
        Callback::from(move |req: PlaylistEpgRequest| {
            epg_set.set(None);
            search_filter.set(SearchRequest::Clear);
            visible_range.set((0, 20));
            pixels_per_min.set(TimelineZoom(DEFAULT_PIXELS_PER_MIN));
            *pending_scroll_left.borrow_mut() = Some(0);
            if let Some(el) = container_ref.cast::<HtmlElement>() {
                el.set_scroll_top(0);
                el.set_scroll_left(0);
            }
            let service_ctx = service_ctx.clone();
            let epg_set = epg_set.clone();
            service_ctx.event.broadcast(EventMessage::Busy(BusyStatus::Show));
            spawn_local(async move {
                let playlist_epg = service_ctx.playlist.get_playlist_epg(req).await;
                service_ctx.event.broadcast(EventMessage::Busy(BusyStatus::Hide));
                set_timeout(
                    move || {
                        epg_set.set(playlist_epg);
                    },
                    16,
                );
            });
        })
    };

    let epg_window = (*epg).as_ref().map(|tv| (tv.start, tv.stop));
    let timeline_zoom = pixels_per_min.0;

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
    let timeline_html = use_memo((epg_window, timeline_zoom), |(epg_window, timeline_zoom)| {
        let Some((start, stop)) = *epg_window else { return html! {} };
        let start_window_secs = (start / (TIME_BLOCK_MINS * 60)) * (TIME_BLOCK_MINS * 60);
        let start_window = (start_window_secs / 60).max(0);
        let end_window = (stop / 60).max(0);
        let window_duration = (end_window - start_window).max(0);
        let num_blocks = (window_duration + TIME_BLOCK_MINS - 1) / TIME_BLOCK_MINS;
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
    });

    {
        let container_ref = container_ref.clone();
        let pending_scroll_left = pending_scroll_left.clone();
        use_effect_with(pixels_per_min.0, move |_| {
            if let Some(scroll_left) = pending_scroll_left.borrow_mut().take() {
                if let Some(container) = container_ref.cast::<HtmlElement>() {
                    container.set_scroll_left(scroll_left);
                }
            }
            || ()
        });
    }

    {
        let container_ref = container_ref.clone();
        let now_line_ref = now_line_ref.clone();
        use_effect_with((epg_window, pixels_per_min.0), move |(epg_window, pixels_per_min)| {
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
            update_now_line(&container_ref, &now_line_ref, epg_window, pixels_per_min.0, true);
            || ()
        });
    }

    let apply_timeline_zoom = {
        let container_ref = container_ref.clone();
        let pending_scroll_left = pending_scroll_left.clone();
        let pixels_per_min = pixels_per_min.clone();
        Callback::from(move |(next_pixels_per_min, anchor_x): (f64, f64)| {
            let current_pixels_per_min = pixels_per_min.0;
            let next_pixels_per_min = clamp_timeline_zoom(next_pixels_per_min);
            if (next_pixels_per_min - current_pixels_per_min).abs() < ZOOM_EQUALITY_TOLERANCE {
                return;
            }

            let scroll_left =
                container_ref.cast::<HtmlElement>().map_or(0.0, |container| f64::from(container.scroll_left()));
            let next_scroll_left =
                compute_zoomed_scroll_left(scroll_left, anchor_x.max(0.0), current_pixels_per_min, next_pixels_per_min);
            *pending_scroll_left.borrow_mut() = Some(next_scroll_left.round() as i32);
            pixels_per_min.set(TimelineZoom(next_pixels_per_min));
        })
    };

    let handle_timeline_wheel = {
        let timeline_ref = timeline_ref.clone();
        let apply_timeline_zoom = apply_timeline_zoom.clone();
        let pixels_per_min = pixels_per_min.clone();
        Callback::from(move |e: WheelEvent| {
            e.prevent_default();
            e.stop_propagation();
            if let Some(timeline) = timeline_ref.cast::<HtmlElement>() {
                let rect = timeline.get_bounding_client_rect();
                let anchor_x = f64::from(e.client_x()) - rect.left();
                let next_pixels_per_min = if e.delta_y() < 0.0 {
                    pixels_per_min.0 * WHEEL_ZOOM_FACTOR_IN
                } else {
                    pixels_per_min.0 * WHEEL_ZOOM_FACTOR_OUT
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
                let midpoint_x = (f64::from(first.client_x()) + f64::from(second.client_x())) / 2.0;

                *pinch_state.borrow_mut() = Some(TimelinePinchState {
                    distance,
                    anchor_x: midpoint_x - rect.left(),
                    pixels_per_min: pixels_per_min.0,
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
            let move_handle: MouseMoveHandle = Rc::new(RefCell::new(None));
            let up_handle: MouseUpHandle = Rc::new(RefCell::new(None));
            if *is_timeline_panning {
                if let Some(win) = window() {
                    let container_ref_for_move = container_ref.clone();
                    let timeline_pan_state_for_move = timeline_pan_state.clone();
                    let mouse_move = Closure::wrap(Box::new(move |e: MouseEvent| {
                        let Some(pan_state) = *timeline_pan_state_for_move.borrow() else {
                            return;
                        };
                        let Some(container) = container_ref_for_move.cast::<HtmlElement>() else {
                            return;
                        };
                        e.prevent_default();
                        container.set_scroll_left(compute_panned_scroll(
                            pan_state.scroll_left,
                            pan_state.pointer_x,
                            e.client_x(),
                        ));
                    }) as Box<dyn FnMut(_)>);
                    let _ = win.add_event_listener_with_callback("mousemove", mouse_move.as_ref().unchecked_ref());
                    *move_handle.borrow_mut() = Some(mouse_move);

                    let timeline_pan_state_for_up = timeline_pan_state.clone();
                    let is_timeline_panning_for_up = is_timeline_panning_handle.clone();
                    let mouse_up = Closure::wrap(Box::new(move |_e: MouseEvent| {
                        *timeline_pan_state_for_up.borrow_mut() = None;
                        is_timeline_panning_for_up.set(false);
                    }) as Box<dyn FnMut(_)>);
                    let _ = win.add_event_listener_with_callback("mouseup", mouse_up.as_ref().unchecked_ref());
                    *up_handle.borrow_mut() = Some(mouse_up);
                }
            }

            move || {
                if let Some(win) = window() {
                    if let Some(mouse_move) = move_handle.borrow_mut().take() {
                        let _ =
                            win.remove_event_listener_with_callback("mousemove", mouse_move.as_ref().unchecked_ref());
                    }
                    if let Some(mouse_up) = up_handle.borrow_mut().take() {
                        let _ = win.remove_event_listener_with_callback("mouseup", mouse_up.as_ref().unchecked_ref());
                    }
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
            let move_handle: MouseMoveHandle = Rc::new(RefCell::new(None));
            let up_handle: MouseUpHandle = Rc::new(RefCell::new(None));
            if *is_program_panning {
                if let Some(win) = window() {
                    let container_ref_for_move = container_ref.clone();
                    let program_pan_state_for_move = program_pan_state.clone();
                    let mouse_move = Closure::wrap(Box::new(move |e: MouseEvent| {
                        let Some(pan_state) = *program_pan_state_for_move.borrow() else {
                            return;
                        };
                        let Some(container) = container_ref_for_move.cast::<HtmlElement>() else {
                            return;
                        };
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
                    }) as Box<dyn FnMut(_)>);
                    let _ = win.add_event_listener_with_callback("mousemove", mouse_move.as_ref().unchecked_ref());
                    *move_handle.borrow_mut() = Some(mouse_move);

                    let program_pan_state_for_up = program_pan_state.clone();
                    let is_program_panning_for_up = is_program_panning_handle.clone();
                    let mouse_up = Closure::wrap(Box::new(move |_e: MouseEvent| {
                        *program_pan_state_for_up.borrow_mut() = None;
                        is_program_panning_for_up.set(false);
                    }) as Box<dyn FnMut(_)>);
                    let _ = win.add_event_listener_with_callback("mouseup", mouse_up.as_ref().unchecked_ref());
                    *up_handle.borrow_mut() = Some(mouse_up);
                }
            }

            move || {
                if let Some(win) = window() {
                    if let Some(mouse_move) = move_handle.borrow_mut().take() {
                        let _ =
                            win.remove_event_listener_with_callback("mousemove", mouse_move.as_ref().unchecked_ref());
                    }
                    if let Some(mouse_up) = up_handle.borrow_mut().take() {
                        let _ = win.remove_event_listener_with_callback("mouseup", mouse_up.as_ref().unchecked_ref());
                    }
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

    let row_height = use_memo((), move |_| {
        let doc = window().unwrap().document().unwrap();
        let root = doc.document_element().unwrap(); // <html>
        let style = window().unwrap().get_computed_style(&root).unwrap().unwrap();

        let row_height = style.get_property_value("--epg-row-height").unwrap_or_else(|_| String::new()); // fallback if not set

        row_height.trim_end_matches("px").parse::<usize>().unwrap_or(60).max(1)
    });

    // Add scroll listener to calculate visible channels
    {
        let container_ref = container_ref.clone();
        let visible_range = visible_range.clone();
        let channel_row_height = *row_height;
        use_effect_with((), move |_| {
            let debounce_handle: Rc<RefCell<Option<Timeout>>> = Rc::new(RefCell::new(None));
            let onscroll_handle: OnScrollHandle = Rc::new(RefCell::new(None));
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
                div.add_event_listener_with_callback("scroll", onscroll.as_ref().unchecked_ref()).unwrap();
                *onscroll_handle_clone.borrow_mut() = Some(onscroll);
            }
            move || {
                if let Some(prev) = debounce_handle.borrow_mut().take() {
                    prev.cancel();
                }
                if let Some(onscroll) = onscroll_handle.borrow_mut().take() {
                    drop(onscroll);
                }
            }
        });
    }

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
                        html! { <NoContent text={translate.t("MESSAGES.EPG.SELECT_AN_EPG_TO_VIEW_CONTENT")}/> }
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
                                            let left = get_pos(p.start, *start_window, pixels_per_min.0);
                                            let right = get_pos(p.stop, *start_window, pixels_per_min.0);
                                            let width = (right - left).max(0);

                                            if let (Some(pstart_time), Some(pend_time)) = (
                                                    Utc.timestamp_opt(p.start, 0).single(),
                                                    Utc.timestamp_opt(p.stop, 0).single()) {
                                                let pstart_time_local = pstart_time.with_timezone(&Local);
                                                let pend_time_local = pend_time.with_timezone(&Local);
                                                let pstart = pstart_time_local.format("%H:%M").to_string();
                                                let pend = pend_time_local.format("%H:%M").to_string();
                                                let program_style = format!("left:{left}px; width:{width}px; min-width:{width}px; max-width:{width}px");

                                                html! {
                                                <div class={classes!("tp__epg__program", if is_active { "tp__epg__program-active" } else {""})} style={program_style} title={ p.title.as_ref().map(ToString::to_string).unwrap_or_default() }>
                                                    <div class="tp__epg__program-time">{ &pstart } {"-"} { &pend }</div>
                                                    <div class="tp__epg__program-title">
                                                        { p.title.as_ref().map(ToString::to_string).unwrap_or_default() }
                                                    </div>
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
