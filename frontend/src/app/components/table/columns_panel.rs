use super::{column_order, visible_columns, TableColumn, TablePanelSpec};
use crate::{
    app::components::{AppIcon, CustomDialog, FieldLabel, TextButton, ToggleSwitch},
    error::Error,
    hooks::use_service_context,
    i18n::use_translation,
    provider::{use_user_settings, UserSettingsAction},
};
use gloo_events::{EventListener, EventListenerOptions};
use gloo_render::{request_animation_frame, AnimationFrame};
use shared::model::TableLayoutPreferencesDto;
use std::{cell::RefCell, rc::Rc};
use wasm_bindgen::{closure::Closure, JsCast};
use web_sys::{Element, HtmlElement, PointerEvent, ResizeObserver};
use yew::prelude::*;

struct DragState {
    id: String,
    original: TableLayoutPreferencesDto,
    pointer: Option<i32>,
    start_y: f64,
    current_y: f64,
    reset_before: bool,
    active: bool,
    dirty: bool,
    scroll: i32,
    row: Option<HtmlElement>,
    grab_offset: f64,
    translation: f64,
}

impl DragState {
    fn follow_pointer(&mut self, list: &HtmlElement) {
        let Some(row) = self.row.as_ref().filter(|_| self.active) else {
            return;
        };
        let bounds = row.get_bounding_client_rect();
        let list_bounds = list.get_bounding_client_rect();
        let top = (self.current_y - self.grab_offset)
            .clamp(list_bounds.top(), (list_bounds.bottom() - bounds.height()).max(list_bounds.top()));
        let translation = top - (bounds.top() - self.translation);
        let _ = row.style().set_property("transform", &format!("translateY({translation}px)"));
        self.translation = translation;
    }

    fn clear_visual(&self) {
        if let Some(row) = self.row.as_ref() {
            let _ = row.style().remove_property("transform");
        }
    }
}

fn full_draft(layout: &TableLayoutPreferencesDto, columns: &[TableColumn]) -> TableLayoutPreferencesDto {
    let mut order = Vec::new();
    for id in &layout.column_order {
        if !order.contains(id) {
            order.push(id.clone());
        }
    }
    for column in columns {
        if !order.iter().any(|id| id == column.id.as_str()) {
            order.push(column.id.to_string());
        }
    }
    TableLayoutPreferencesDto { column_order: order, column_visibility: layout.column_visibility.clone() }
}

fn move_column(layout: &TableLayoutPreferencesDto, id: &str, target: &str) -> TableLayoutPreferencesDto {
    let mut result = layout.clone();
    let Some(from) = result.column_order.iter().position(|value| value == id) else {
        return result;
    };
    let Some(to) = result.column_order.iter().position(|value| value == target) else {
        return result;
    };
    let value = result.column_order.remove(from);
    result.column_order.insert(to, value);
    result
}

fn pointer_layout(
    layout: &TableLayoutPreferencesDto,
    id: &str,
    list: &Element,
    y: f64,
) -> Option<TableLayoutPreferencesDto> {
    let nodes = list.query_selector_all("[data-column-id]").ok()?;
    let mut before = None;
    let mut last = None;
    for i in 0..nodes.length() {
        let row = nodes.item(i)?.dyn_into::<Element>().ok()?;
        let candidate = row.get_attribute("data-column-id")?;
        if candidate == id {
            continue;
        }
        let rect = row.get_bounding_client_rect();
        if y <= rect.top() + rect.height() / 2.0 {
            before = Some(candidate);
            break;
        }
        last = Some(candidate);
    }
    let position = if let Some(before) = before {
        layout
            .column_order
            .iter()
            .filter(|candidate| candidate.as_str() != id)
            .position(|candidate| candidate == &before)?
    } else {
        layout
            .column_order
            .iter()
            .filter(|candidate| candidate.as_str() != id)
            .position(|candidate| Some(candidate) == last.as_ref())?
            + 1
    };
    reordered_layout(layout, id, position)
}

fn reordered_layout(
    layout: &TableLayoutPreferencesDto,
    id: &str,
    position: usize,
) -> Option<TableLayoutPreferencesDto> {
    let from = layout.column_order.iter().position(|candidate| candidate == id)?;
    if from == position || position >= layout.column_order.len() {
        return None;
    }
    let mut result = layout.clone();
    let value = result.column_order.remove(from);
    result.column_order.insert(position, value);
    Some(result)
}

fn schedule_drag_frame(frame: &Rc<RefCell<Option<AnimationFrame>>>, tick: Rc<dyn Fn()>) {
    let next_frame = Rc::downgrade(frame);
    *frame.borrow_mut() = Some(request_animation_frame(move |_| {
        if let Some(frame) = next_frame.upgrade() {
            tick();
            schedule_drag_frame(&frame, tick);
        }
    }));
}

const PANEL_GUTTER: f64 = 8.0;

#[derive(Clone, Copy)]
struct PanelBounds {
    top: f64,
    left: f64,
    width: f64,
    height: f64,
}

fn panel_position(anchor: PanelBounds, size: (f64, f64), viewport: PanelBounds, rtl: bool) -> (f64, f64) {
    let min_left = viewport.left + PANEL_GUTTER;
    let max_left = (viewport.left + viewport.width - size.0 - PANEL_GUTTER).max(min_left);
    let left_side = anchor.left - size.0 - PANEL_GUTTER;
    let right_side = anchor.left + anchor.width + PANEL_GUTTER;
    let (preferred, alternative) = if rtl { (right_side, left_side) } else { (left_side, right_side) };
    let left = if (min_left..=max_left).contains(&preferred) {
        preferred
    } else if (min_left..=max_left).contains(&alternative) {
        alternative
    } else {
        preferred.clamp(min_left, max_left)
    };
    let min_top = viewport.top + PANEL_GUTTER;
    let max_top = (viewport.top + viewport.height - size.1 - PANEL_GUTTER).max(min_top);
    (anchor.top.clamp(min_top, max_top), left)
}

#[hook]
fn use_panel_anchor(panel_ref: NodeRef, anchor: Option<Element>, on_close: Callback<()>) {
    use_effect_with(anchor, move |anchor| {
        let observed_anchor = anchor.clone();
        let observed_panel = panel_ref.clone();
        let anchor = anchor.clone();
        let update: Rc<dyn Fn()> = Rc::new(move || {
            let Some(anchor) = anchor.as_ref() else {
                return;
            };
            if !anchor.is_connected() {
                on_close.emit(());
                return;
            }
            let Some(window) = web_sys::window() else {
                return;
            };
            let Some(panel) = panel_ref.cast::<HtmlElement>() else {
                return;
            };
            let viewport = if let Some(viewport) = window.visual_viewport() {
                PanelBounds {
                    top: viewport.offset_top(),
                    left: viewport.offset_left(),
                    width: viewport.width(),
                    height: viewport.height(),
                }
            } else if let Some(root) = window.document().and_then(|document| document.document_element()) {
                PanelBounds {
                    top: 0.0,
                    left: 0.0,
                    width: f64::from(root.client_width()),
                    height: f64::from(root.client_height()),
                }
            } else {
                return;
            };
            let style = panel.style();
            let _ = style.set_property(
                "--table-columns-max-width",
                &format!("{}px", (viewport.width - 2.0 * PANEL_GUTTER).max(1.0)),
            );
            let _ = style.set_property(
                "--table-columns-max-height",
                &format!("{}px", (viewport.height - 2.0 * PANEL_GUTTER).max(1.0)),
            );
            let bounds = anchor.get_bounding_client_rect();
            let size = panel.get_bounding_client_rect();
            let rtl = window
                .get_computed_style(anchor)
                .ok()
                .flatten()
                .and_then(|style| style.get_property_value("direction").ok())
                .is_some_and(|direction| direction == "rtl");
            let (top, left) = panel_position(
                PanelBounds { top: bounds.top(), left: bounds.left(), width: bounds.width(), height: bounds.height() },
                (size.width(), size.height()),
                viewport,
                rtl,
            );
            let _ = style.set_property("--table-columns-top", &format!("{top}px"));
            let _ = style.set_property("--table-columns-left", &format!("{left}px"));
            let _ = style.set_property("--table-columns-inline-end", "auto");
        });
        update();
        let resize = {
            let update = update.clone();
            Closure::<dyn FnMut(js_sys::Array, ResizeObserver)>::new(move |_, _| update())
        };
        let observer = ResizeObserver::new(resize.as_ref().unchecked_ref()).ok();
        if let Some(observer) = observer.as_ref() {
            if let Some(panel) = observed_panel.cast::<Element>() {
                observer.observe(&panel);
            }
            if let Some(anchor) = observed_anchor {
                observer.observe(&anchor);
            }
        }
        let mut listeners = Vec::new();
        if let Some(window) = web_sys::window() {
            let on_resize = update.clone();
            listeners.push(EventListener::new(&window, "resize", move |_| on_resize()));
            let on_scroll = update.clone();
            listeners.push(EventListener::new_with_options(
                &window,
                "scroll",
                EventListenerOptions::run_in_capture_phase(),
                move |_| on_scroll(),
            ));
            if let Some(viewport) = window.visual_viewport() {
                for event in ["resize", "scroll"] {
                    let update = update.clone();
                    listeners.push(EventListener::new(&viewport, event, move |_| update()));
                }
            }
        }
        move || {
            if let Some(observer) = observer {
                observer.disconnect();
            }
            drop(resize);
            drop(listeners);
        }
    });
}

#[derive(Properties, PartialEq)]
pub struct TableColumnsPanelProps {
    pub spec: TablePanelSpec,
    pub on_close: Callback<()>,
}

#[component]
pub fn TableColumnsPanel(props: &TableColumnsPanelProps) -> Html {
    let services = use_service_context();
    let context = use_user_settings();
    let translate = use_translation();
    let draft = use_state(TableLayoutPreferencesDto::default);
    let draft_ref = use_mut_ref(TableLayoutPreferencesDto::default);
    let etag = use_state(|| None::<String>);
    let pending = use_state(|| false);
    let error = use_state(|| None::<Error>);
    let reset = use_state(|| false);
    let conflict = use_state(|| None::<crate::services::TableLayoutSection>);
    let announcement = use_state(String::new);
    let drag = use_mut_ref(|| None::<DragState>);
    let dragging = use_state_eq(|| None::<String>);
    let pointer_drag = use_state_eq(|| false);
    let list = use_node_ref();
    let panel_ref = use_node_ref();
    use_panel_anchor(panel_ref.clone(), props.spec.anchor.clone(), props.on_close.clone());
    let generation = services.auth.session_generation();
    let apply = {
        let draft = draft.clone();
        let draft_ref = draft_ref.clone();
        Callback::from(move |value: TableLayoutPreferencesDto| {
            *draft_ref.borrow_mut() = value.clone();
            draft.set(value);
        })
    };
    {
        let services = services.clone();
        let spec = props.spec.clone();
        let context = context.clone();
        let apply = apply.clone();
        let etag = etag.clone();
        let error = error.clone();
        let pending = pending.clone();
        use_effect_with(props.spec.table_id.clone(), move |_| {
            if spec.supported {
                pending.set(true);
                yew::platform::spawn_local(async move {
                    let result = services.user_settings.table(spec.table_id.as_str()).await;
                    if services.auth.session_generation() != generation {
                        return;
                    }
                    match result {
                        Ok(section) => {
                            apply.emit(full_draft(&section.layout, &spec.columns));
                            etag.set(Some(section.etag.clone()));
                            if let Some(context) = context {
                                context.state.dispatch(UserSettingsAction::Section(
                                    generation,
                                    spec.table_id.to_string(),
                                    section,
                                ));
                            }
                        }
                        Err(err) => error.set(Some(err)),
                    }
                    pending.set(false);
                });
            }
            || ()
        });
    }
    {
        let list = list.clone();
        let drag = drag.clone();
        let draft_ref = draft_ref.clone();
        let apply = apply.clone();
        let reset = reset.clone();
        use_effect_with(*pointer_drag, move |active| {
            let frame = active.then(|| {
                let frame = Rc::new(RefCell::new(None));
                let tick = Rc::new(move || {
                    if let Some(state) =
                        drag.borrow_mut().as_mut().filter(|state| state.active && state.pointer.is_some())
                    {
                        if let Some(list) = list.cast::<HtmlElement>() {
                            if state.scroll != 0 {
                                list.set_scroll_top(list.scroll_top() + state.scroll);
                            }
                            if state.dirty || state.scroll != 0 {
                                state.dirty = false;
                                let next = pointer_layout(&draft_ref.borrow(), &state.id, &list, state.current_y);
                                if let Some(next) = next {
                                    apply.emit(next);
                                    reset.set(false);
                                }
                                state.follow_pointer(&list);
                            }
                        }
                    }
                });
                schedule_drag_frame(&frame, tick);
                frame
            });
            move || drop(frame)
        });
    }
    {
        let drag = drag.clone();
        let list = list.clone();
        use_effect(move || {
            if let Some(state) = drag.borrow_mut().as_mut().filter(|state| state.pointer.is_some()) {
                if let Some(list) = list.cast::<HtmlElement>() {
                    state.follow_pointer(&list);
                }
            }
            || ()
        });
    }
    let on_reset = {
        let apply = apply.clone();
        let reset = reset.clone();
        let columns = props.spec.columns.clone();
        Callback::from(move |_| {
            apply.emit(full_draft(&TableLayoutPreferencesDto::default(), &columns));
            reset.set(true);
        })
    };
    let on_save = {
        let services = services.clone();
        let context = context.clone();
        let spec = props.spec.clone();
        let draft = draft.clone();
        let etag = etag.clone();
        let pending = pending.clone();
        let error = error.clone();
        let reset = reset.clone();
        let conflict = conflict.clone();
        let close = props.on_close.clone();
        Callback::from(move |_| {
            let Some(etag) = (*etag).clone() else {
                return;
            };
            let services = services.clone();
            let context = context.clone();
            let spec = spec.clone();
            let layout = (*draft).clone();
            let pending = pending.clone();
            let error = error.clone();
            let conflict = conflict.clone();
            let close = close.clone();
            let reset = *reset;
            pending.set(true);
            error.set(None);
            yew::platform::spawn_local(async move {
                let result = if reset {
                    services.user_settings.reset(spec.table_id.as_str(), &etag).await
                } else {
                    services.user_settings.save(spec.table_id.as_str(), layout, &etag).await
                };
                if services.auth.session_generation() != generation {
                    return;
                }
                match result {
                    Ok(section) => {
                        if let Some(context) = context {
                            context.state.dispatch(UserSettingsAction::Section(
                                generation,
                                spec.table_id.to_string(),
                                section,
                            ));
                        }
                        close.emit(());
                    }
                    Err(err) => {
                        if matches!(err, Error::PreconditionFailed(_)) {
                            if let Ok(section) = services.user_settings.table(spec.table_id.as_str()).await {
                                if services.auth.session_generation() == generation {
                                    conflict.set(Some(section));
                                }
                            }
                        }
                        error.set(Some(err));
                    }
                }
                pending.set(false);
            });
        })
    };
    let reapply = {
        let conflict = conflict.clone();
        let etag = etag.clone();
        let error = error.clone();
        Callback::from(move |_| {
            if let Some(section) = conflict.as_ref() {
                etag.set(Some(section.etag.clone()));
            }
            conflict.set(None);
            error.set(None);
        })
    };
    let close = {
        let on_close = props.on_close.clone();
        let pending = pending.clone();
        Callback::from(move |()| {
            if !*pending {
                on_close.emit(());
            }
        })
    };
    let cancel_drag = {
        let pointer_drag = pointer_drag.clone();
        let drag = drag.clone();
        let dragging = dragging.clone();
        let apply = apply.clone();
        let reset = reset.clone();
        Callback::from(move |()| {
            if let Some(state) = drag.borrow_mut().take() {
                state.clear_visual();
                pointer_drag.set(false);
                apply.emit(state.original);
                reset.set(state.reset_before);
                dragging.set(None);
            }
        })
    };
    let dismiss = {
        let drag = drag.clone();
        let cancel_drag = cancel_drag.clone();
        let close = close.clone();
        Callback::from(move |()| {
            let active = drag.borrow().is_some();
            if active {
                cancel_drag.emit(());
            } else {
                close.emit(());
            }
        })
    };
    let cancel = {
        let cancel_drag = cancel_drag.clone();
        Callback::from(move |_: PointerEvent| cancel_drag.emit(()))
    };
    let hint_key = if context.as_ref().is_some_and(|context| context.state.settings.shared) {
        "TABLE_COLUMNS_SHARED"
    } else {
        "TABLE_COLUMNS"
    };
    let order = column_order(&props.spec.columns, &draft);
    let visible = visible_columns(&props.spec.columns, &draft);
    let pointermove = {
        let pointer_drag = pointer_drag.clone();
        let drag = drag.clone();
        let list = list.clone();
        let dragging = dragging.clone();
        Callback::from(move |event: PointerEvent| {
            let Some(list) = list.cast::<Element>() else {
                return;
            };
            let mut state = drag.borrow_mut();
            let Some(state) = state.as_mut().filter(|state| state.pointer == Some(event.pointer_id())) else {
                return;
            };
            let y = f64::from(event.client_y());
            if !state.active && (y - state.start_y).abs() < 5.0 {
                return;
            }
            state.current_y = y;
            state.dirty = true;
            if !state.active {
                state.active = true;
                pointer_drag.set(true);
                dragging.set(Some(state.id.clone()));
            }
            let bounds = list.get_bounding_client_rect();
            state.scroll = if y < bounds.top() + 32.0 {
                -8
            } else if y > bounds.bottom() - 32.0 {
                8
            } else {
                0
            };
        })
    };
    let pointerup = {
        let pointer_drag = pointer_drag.clone();
        let draft_ref = draft_ref.clone();
        let apply = apply.clone();
        let reset = reset.clone();
        let drag = drag.clone();
        let dragging = dragging.clone();
        let list = list.clone();
        Callback::from(move |event: PointerEvent| {
            if drag.borrow().as_ref().is_some_and(|state| state.pointer == Some(event.pointer_id())) {
                if let Some(state) = drag.borrow_mut().take() {
                    if state.active {
                        if let Some(list) = list.cast::<Element>() {
                            let next =
                                pointer_layout(&draft_ref.borrow(), &state.id, &list, f64::from(event.client_y()));
                            if let Some(next) = next {
                                apply.emit(next);
                                reset.set(false);
                            }
                        }
                    }
                    state.clear_visual();
                }
                pointer_drag.set(false);
                dragging.set(None);
            }
            if let Some(handle) = list.cast::<Element>() {
                let _ = handle.release_pointer_capture(event.pointer_id());
            }
        })
    };
    let entries = order.iter().map(|&index| {
        let column = &props.spec.columns[index];
        let id = column.id.to_string();
        let label = column.label.resolve(|key| translate.t(key));
        let checked = visible.contains(&index);
        let last_content = checked && column.content && visible.iter().filter(|i| props.spec.columns[**i].content).count() <= 1;
        let toggle = {
            let id = id.clone(); let draft_ref = draft_ref.clone(); let apply = apply.clone(); let reset = reset.clone();
            Callback::from(move |checked: bool| {
                let mut value = draft_ref.borrow().clone(); value.column_visibility.insert(id.clone(), checked);
                apply.emit(value); reset.set(false);
            })
        };
        let keydown = {
            let id = id.clone(); let columns = props.spec.columns.clone(); let draft_ref = draft_ref.clone(); let drag = drag.clone();
            let dragging = dragging.clone(); let apply = apply.clone(); let reset = reset.clone(); let cancel_drag = cancel_drag.clone();
            let pointer_drag = pointer_drag.clone();
            let announcement = announcement.clone(); let translate = translate.clone(); let label = label.clone();
            Callback::from(move |event: KeyboardEvent| {
                let key = event.key();
                if key == "Escape" && drag.borrow().is_some() { event.prevent_default(); event.stop_propagation(); cancel_drag.emit(()); return; }
                if key == " " || key == "Enter" {
                    event.prevent_default(); event.stop_propagation();
                    if drag.borrow().is_some() { if let Some(state) = drag.borrow_mut().take() { state.clear_visual(); } pointer_drag.set(false); dragging.set(None); }
                    else { *drag.borrow_mut() = Some(DragState { id: id.clone(), original: draft_ref.borrow().clone(), pointer: None, start_y: 0.0, current_y: 0.0, reset_before: *reset, active: true, dirty: false, scroll: 0, row: None, grab_offset: 0.0, translation: 0.0 }); dragging.set(Some(id.clone())); }
                    return;
                }
                if matches!(key.as_str(), "ArrowUp" | "ArrowDown") && drag.borrow().as_ref().is_some_and(|state| state.id == id && state.pointer.is_none()) {
                    event.prevent_default(); event.stop_propagation();
                    let current = draft_ref.borrow().clone(); let order = column_order(&columns, &current);
                    if let Some(position) = order.iter().position(|index| columns[*index].id.as_str() == id) {
                        let next = if key == "ArrowUp" { position.checked_sub(1) } else { Some(position + 1).filter(|next| *next < order.len()) };
                        if let Some(next) = next {
                            apply.emit(move_column(&current, &id, columns[order[next]].id.as_str())); reset.set(false);
                            announcement.set(translate.t("TABLE_COLUMNS.POSITION").replace("{column}", &label).replace("{position}", &(next + 1).to_string()).replace("{total}", &order.len().to_string()));
                        }
                    }
                }
            })
        };
        let pointerdown = {
            let id = id.clone(); let drag = drag.clone(); let draft_ref = draft_ref.clone(); let reset = reset.clone(); let list = list.clone(); let dragging = dragging.clone();
            Callback::from(move |event: PointerEvent| {
                if event.button() != 0 || !event.is_primary() || drag.borrow().is_some() { return; }
                event.prevent_default();
                if let Some(list) = list.cast::<Element>() {
                    let _ = list.set_pointer_capture(event.pointer_id());
                }
                if let Some(handle) = event.target().and_then(|target| target.dyn_into::<Element>().ok())
                    .and_then(|target| target.closest(".tp__table-columns__handle").ok().flatten()) {
                    if let Some(element) = handle.dyn_ref::<HtmlElement>() { let _ = element.focus(); }
                }
                let row = event.target().and_then(|target| target.dyn_into::<Element>().ok())
                    .and_then(|target| target.closest("[data-column-id]").ok().flatten())
                    .and_then(|row| row.dyn_into::<HtmlElement>().ok());
                let grab_offset = row.as_ref().map_or(0.0, |row| f64::from(event.client_y()) - row.get_bounding_client_rect().top());
                *drag.borrow_mut() = Some(DragState { id: id.clone(), original: draft_ref.borrow().clone(), pointer: Some(event.pointer_id()), start_y: f64::from(event.client_y()), current_y: f64::from(event.client_y()), reset_before: *reset, active: false, dirty: false, scroll: 0, row, grab_offset, translation: 0.0 });
                dragging.set(Some(id.clone()));
            })
        };
        html! { <li key={column.id.as_str()} data-column-id={column.id.clone()} class={classes!("tp__table-columns__entry", ((*dragging).as_deref() == Some(&id)).then_some("tp__table-columns__entry--moving"))}>
            <button type="button" class="tp__table-columns__handle tp__icon-button" disabled={!column.can_reorder || *pending}
                aria-label={translate.t("TABLE_COLUMNS.REORDER").replace("{column}", &label)} title={translate.t("TABLE_COLUMNS.KEYBOARD_HELP")}
                onkeydown={keydown} onpointerdown={pointerdown}>
                <AppIcon name="DragHandle"/>
            </button>
            <span class="tp__table-columns__label" title={label.clone()}>{label.clone()}
                if !column.can_hide { <small class="tp__table-columns__required">{translate.t("TABLE_COLUMNS.REQUIRED")}</small> }
            </span>
            if column.can_hide {
                <ToggleSwitch compact=true value={checked} readonly={last_content || *pending} aria_label={Some(label.to_string())} on_change={toggle}/>
            }
        </li> }
    }).collect::<Html>();
    let retry = {
        let services = services.clone();
        let spec = props.spec.clone();
        let apply = apply.clone();
        let etag = etag.clone();
        let error = error.clone();
        let pending = pending.clone();
        Callback::from(move |_| {
            let services = services.clone();
            let spec = spec.clone();
            let apply = apply.clone();
            let etag = etag.clone();
            let error = error.clone();
            let pending = pending.clone();
            pending.set(true);
            yew::platform::spawn_local(async move {
                let result = services.user_settings.table(spec.table_id.as_str()).await;
                if services.auth.session_generation() != generation {
                    return;
                }
                match result {
                    Ok(section) => {
                        apply.emit(full_draft(&section.layout, &spec.columns));
                        etag.set(Some(section.etag));
                        error.set(None);
                    }
                    Err(err) => error.set(Some(err)),
                }
                pending.set(false);
            });
        })
    };
    let error_text = error.as_ref().map(|err| match err {
        Error::PreconditionFailed(_) => translate.t("TABLE_COLUMNS.CONFLICT"),
        Error::BadRequest(code) | Error::Conflict(code) | Error::InternalServerError(code) => {
            let key = format!("TABLE_COLUMNS.ERROR_{}", code.trim_start_matches("settings_").to_uppercase());
            let text = translate.t(&key);
            if text == key {
                translate.t("TABLE_COLUMNS.ERROR")
            } else {
                text
            }
        }
        _ => translate.t("TABLE_COLUMNS.ERROR"),
    });
    let cancel_button = {
        let close = close.clone();
        Callback::from(move |_| close.emit(()))
    };
    let content = html! { <CustomDialog node_ref={Some(panel_ref)} class={Some("tp__table-columns".to_owned())} open={true} modal={true} close_on_backdrop_click={true}
        on_close={Some(dismiss)} aria_label={Some(translate.t("TABLE_COLUMNS.TITLE"))}>
        <header><FieldLabel label={translate.t("TABLE_COLUMNS.TITLE")} field_id="TABLE_COLUMNS" hint_key={Some(hint_key.to_owned())}/></header>
        if props.spec.supported {
            if let Some(text) = error_text { <p role="alert">{text}</p> }
            <ul ref={list} class={classes!("tp__table-columns__list", dragging.is_some().then_some("tp__table-columns__list--dragging"))} onpointermove={pointermove} onpointerup={pointerup}
                onpointercancel={cancel.clone()} onlostpointercapture={cancel}>{entries}</ul>
            <div aria-live="polite" class="tp__table-columns__live">{(*announcement).clone()}</div>
            <footer>
                if etag.is_none() && !*pending { <TextButton name="retry" icon="Refresh" title={translate.t("TABLE_COLUMNS.RETRY")} onclick={retry}/> }
                if conflict.is_some() { <TextButton name="reapply" class="tp__table-columns__reapply" onclick={reapply} title={translate.t("TABLE_COLUMNS.REAPPLY")}/> }
                <TextButton name="reset" class={classes!("tp__table-columns__reset", (*pending || etag.is_none()).then_some("disabled")).to_string()}
                    icon="Refresh" onclick={on_reset} disabled={*pending || etag.is_none()} title={translate.t("TABLE_COLUMNS.RESET")}/>
                <TextButton name="cancel" class={classes!("secondary", "tp__table-columns__cancel", (*pending).then_some("disabled")).to_string()}
                    icon="Cancel" onclick={cancel_button} disabled={*pending} title={translate.t("TABLE_COLUMNS.CANCEL")}/>
                <TextButton name="save" class={classes!("primary", "tp__table-columns__save", (*pending || etag.is_none() || conflict.is_some() || dragging.is_some()).then_some("disabled")).to_string()}
                    icon="Save" onclick={on_save} disabled={*pending || etag.is_none() || conflict.is_some() || dragging.is_some()} title={translate.t("TABLE_COLUMNS.SAVE")}/>
            </footer>
        } else { <p>{translate.t("TABLE_COLUMNS.LIMIT")}</p> }
    </CustomDialog> };
    if let Some(body) = web_sys::window().and_then(|window| window.document()).and_then(|document| document.body()) {
        yew::create_portal(content, body.into())
    } else {
        content
    }
}

#[cfg(test)]
#[path = "columns_panel.test.rs"]
mod tests;

#[cfg(all(test, target_arch = "wasm32"))]
#[path = "columns_panel.browser.test.rs"]
mod browser_tests;
