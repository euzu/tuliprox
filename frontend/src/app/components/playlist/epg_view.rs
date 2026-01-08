use crate::app::components::{Breadcrumbs, EpgSourceSelector, NoContent};
use crate::hooks::use_service_context;
use crate::model::{BusyStatus, EventMessage};
use crate::utils::set_timeout;
use chrono::{Datelike, Local, TimeZone, Utc};
use gloo_timers::callback::{Interval, Timeout};
use shared::model::{EpgTv, PlaylistEpgRequest};
use std::cell::RefCell;
use std::rc::Rc;
use wasm_bindgen::prelude::Closure;
use wasm_bindgen::JsCast;
use web_sys::{window, HtmlElement};
use yew::platform::spawn_local;
use yew::{classes, function_component, html, use_effect_with, use_memo, use_node_ref, use_state, Callback, Html, UseStateHandle};
use yew_i18n::use_translation;

const TIME_BLOCK_WIDTH: f64 = 210.0;
const TIME_BLOCK_MINS: i64 = 30;
const PIXEL_PER_MIN: f64 = TIME_BLOCK_WIDTH / TIME_BLOCK_MINS as f64;

fn get_pos(secs: i64, start_mins: i64) -> i64 {
    let mins = secs / 60;
    let rel_mins = mins - start_mins;
    (rel_mins as f64 * PIXEL_PER_MIN).round() as i64
}

type OnScrollHandle = Rc<RefCell<Option<Closure<dyn FnMut(web_sys::Event)>>>>;

#[function_component]
pub fn EpgView() -> Html {
    let services = use_service_context();
    let translate = use_translation();
    let epg = use_state::<Option<EpgTv>, _>(|| None);
    let breadcrumbs = use_state(|| Rc::new(vec![translate.t("LABEL.PLAYLISTS"), translate.t("LABEL.PLAYLIST_EPG")]));
    let container_ref = use_node_ref();
    let now_line_ref = use_node_ref();

    // State to keep track of visible channel range
    let visible_range = use_state(|| (0, 20)); // (start_index, end_index)

    let handle_select_source = {
        let service_ctx = services.clone();
        let epg_set = epg.clone();
        Callback::from(move |req: PlaylistEpgRequest| {
            epg_set.set(None);
            let service_ctx = service_ctx.clone();
            let epg_set = epg_set.clone();
            service_ctx.event.broadcast(EventMessage::Busy(BusyStatus::Show));
            spawn_local(async move {
                let playlist_epg = service_ctx.playlist.get_playlist_epg(req).await;
                service_ctx.event.broadcast(EventMessage::Busy(BusyStatus::Hide));
                set_timeout(move || { epg_set.set(playlist_epg); }, 16);
            });
        })
    };

    {
        let container_ref = container_ref.clone();
        let now_line_ref = now_line_ref.clone();
        use_effect_with(epg.clone(), move |epg_tv| {
            // Updates the now-line position
            let epg_tv_clone = epg_tv.clone();

            let calculate_position = Rc::new(move |epg_tv: &UseStateHandle<Option<EpgTv>>, recenter: bool| {
                if let Some(tv) = &**epg_tv {
                    if let (Some(div), Some(now_line)) = (container_ref.cast::<HtmlElement>(), now_line_ref.cast::<HtmlElement>()) {
                        let now = Utc::now().timestamp();
                        if now >= tv.start && now <= tv.stop {
                            let start_window_secs = (tv.start / (TIME_BLOCK_MINS * 60)) * (TIME_BLOCK_MINS * 60);
                            let start_window = (start_window_secs / 60).max(0);
                            let now_line_pos = get_pos(now, start_window);
                            now_line.style().set_property("width", &format!("{now_line_pos}px")).unwrap();
                            now_line.style().set_property("display", "block").unwrap();
                            if recenter {
                                let container_width = div.client_width();
                                let scroll_pos = (now_line_pos as i32 - (container_width >> 1)).max(0);
                                div.set_scroll_left(scroll_pos);
                            }
                        } else {
                            now_line.style().set_property("display", "none").unwrap();
                        }
                    }
                }
            });

            let calculate_pos = calculate_position.clone();
            let interval = Interval::new(60_000, move || {
                calculate_pos(&epg_tv_clone, false);
            });
            calculate_position(epg_tv, true);
            || drop(interval)
        });
    }

    let row_height = use_memo((), move |_| {
        let doc = window().unwrap().document().unwrap();
        let root = doc.document_element().unwrap(); // <html>
        let style = window().unwrap().get_computed_style(&root).unwrap().unwrap();

        let row_height = style.get_property_value("--epg-row-height")
            .unwrap_or_else(|_| String::new()); // fallback if not set


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
                    let handle = Timeout::new(16, move || {
                        if let Some(div) = container_ref.cast::<HtmlElement>() {
                            let scroll_top = div.scroll_top();
                            let client_height = div.client_height(); // Calculate which channel rows are visible

                            // render 10 + 10 more lines
                            let start_index = (scroll_top / (channel_row_height as i32) - 10).max(0);
                            let end_index = ((scroll_top + client_height) / (channel_row_height as i32) + 10).max(0);
                            vr.set((start_index as usize, end_index as usize));
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
            <Breadcrumbs items={&*breadcrumbs}/>
            <div class="tp__epg__header">
                <h1>{translate.t("LABEL.PLAYLIST_EPG")}</h1>
            </div>
            <EpgSourceSelector on_select={handle_select_source} />
            <div class="tp__epg__body" ref={container_ref}>
                {
                    if epg.is_none() {
                        html! { <NoContent /> }
                   } else {
                        let tv = epg.as_ref().unwrap();
                        let start_window_secs = (tv.start / (TIME_BLOCK_MINS*60)) * (TIME_BLOCK_MINS*60);
                        let start_window =  (start_window_secs / 60).max(0);
                        let end_window = (tv.stop / 60).max(0);
                        let window_duration = (end_window - start_window).max(0);
                        let num_blocks = (window_duration + TIME_BLOCK_MINS - 1) / TIME_BLOCK_MINS;
                        let now = Utc::now().timestamp();
                        let block_style= format!("width:{TIME_BLOCK_WIDTH}px; min-width:{TIME_BLOCK_WIDTH}px; max-width:{TIME_BLOCK_WIDTH}px");

                        let (start_index, end_index) = *visible_range;
                        let total_channels = tv.channels.len();
                        let channel_row_height = *row_height;

                        html! {
                        <>
                        <div class="tp__epg__channels">
                            <div class="tp__epg__channels-header"></div>
                            <div style={format!("height:{}px", start_index * channel_row_height)}></div>
                            { for tv.channels.iter().enumerate().skip(start_index).take(end_index - start_index).map(|(_i, ch)| {
                                html! {
                                    <div class="tp__epg__channel" title={ch.title.clone()}
                                         style={format!("max-height:{channel_row_height}px;min-height:{channel_row_height}px;height:{channel_row_height}px")}>
                                        <div class="tp__epg__channel-icon">
                                            { if let Some(icon) = &ch.icon {
                                                html! { <img src={icon.clone()} alt={ch.title.clone()} /> }
                                              } else { html!{} }
                                            }
                                        </div>
                                        <div class="tp__epg__channel-title">{ &ch.title }</div>
                                    </div>
                                }
                              })
                            }
                            <div style={format!("height:{}px", (total_channels.saturating_sub(end_index)) * channel_row_height)}></div>
                        </div>

                        <div class="tp__epg__programs">
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

                            <div style={format!("height:{}px", start_index * channel_row_height)}></div>
                            { for tv.channels.iter().enumerate().skip(start_index).take(end_index - start_index).map(|(_i, ch)| {
                                html! {
                                  <div class="tp__epg__channel-programs" style={format!("max-height:{channel_row_height}px;min-height:{channel_row_height}px;height:{channel_row_height}px")}>
                                    { for ch.programmes.iter().map(|p| {
                                        let is_active = now >= p.start && now < p.stop;
                                        let left = get_pos(p.start, start_window);
                                        let right = get_pos(p.stop, start_window);
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
                                            <div class={classes!("tp__epg__program", if is_active { "tp__epg__program-active" } else {""})} style={program_style} title={ p.title.clone() }>
                                                <div class="tp__epg__program-time">{ &pstart } {"-"} { &pend }</div>
                                                <div class="tp__epg__program-title">{ &p.title }</div>
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
                        </>
                     }
                   }
                }
            </div>
        </div>
    }
}
