use crate::{
    app::{
        components::{Breadcrumbs, Card, NoContent, PlaylistContext, TextButton},
        ConfigContext,
    },
    hooks::use_service_context,
    html_if,
    i18n::use_translation,
    model::EventMessage,
};
use shared::model::{permission::Permission, ConfigTargetDto, LibraryScanSummary};
use std::rc::Rc;
use wasm_bindgen::JsCast;
use web_sys::HtmlElement;
use yew::{platform::spawn_local, prelude::*};
use yew_hooks::use_list;

const LABEL_UPDATE_LOCAL_LIBRARY: &str = "LABEL.UPDATE_LOCAL_LIBRARY";
const LABEL_FORCE: &str = "LABEL.FORCE";
const ACTION_UPDATE_LIBRARY: &str = "update_library";
const ACTION_UPDATE_LIBRARY_FORCE: &str = "update_library_force";
const MAX_LOG_LINES: usize = 500;

fn format_hms_now() -> String {
    let now = js_sys::Date::new_0();
    let pad = |n: u32| if n < 10 { format!("0{n}") } else { n.to_string() };
    format!("{}:{}:{}", pad(now.get_hours()), pad(now.get_minutes()), pad(now.get_seconds()))
}

fn format_library_log_line(summary: &LibraryScanSummary) -> String { summary.message.clone() }

fn append_log_line_entries(current: &[AttrValue], line: String) -> Vec<AttrValue> {
    let mut updated = current.to_vec();
    updated.push(AttrValue::from(line));
    if updated.len() > MAX_LOG_LINES {
        let drop_count = updated.len() - MAX_LOG_LINES;
        updated.drain(0..drop_count);
    }
    updated
}

#[derive(Clone, PartialEq)]
struct LogLinesState {
    lines: Vec<AttrValue>,
}

enum LogLinesAction {
    Clear,
    Append(String),
}

impl Reducible for LogLinesState {
    type Action = LogLinesAction;

    fn reduce(self: Rc<Self>, action: Self::Action) -> Rc<Self> {
        match action {
            LogLinesAction::Clear => Rc::new(Self { lines: Vec::new() }),
            LogLinesAction::Append(line) => Rc::new(Self { lines: append_log_line_entries(&self.lines, line) }),
        }
    }
}

fn push_log_line(lines: &UseReducerHandle<LogLinesState>, line: String) {
    lines.dispatch(LogLinesAction::Append(line));
}

#[component]
pub fn PlaylistUpdateView() -> Html {
    let translate = use_translation();
    let playlist_ctx = use_context::<PlaylistContext>().expect("Playlist context not found");
    let config_ctx = use_context::<ConfigContext>().expect("Config context not found");
    let services_ctx = use_service_context();
    let can_write_playlist = services_ctx.auth.has_permission(Permission::PlaylistWrite);
    let can_write_library = services_ctx.auth.has_permission(Permission::LibraryWrite);
    let breadcrumbs = use_state(|| Rc::new(vec![translate.t("LABEL.PLAYLISTS"), translate.t("LABEL.UPDATE")]));
    let selected_targets = use_list::<Rc<ConfigTargetDto>>(vec![]);
    let updating = use_state(|| false);
    let library_updating = use_state(|| false);
    let log_lines = use_reducer(|| LogLinesState { lines: Vec::new() });
    let log_container_ref = use_node_ref();

    // Subscribe to playlist and library progress events. The subscription lives for
    // the component lifetime; cleanup unsubscribes on unmount to avoid leaks when
    // the user navigates away mid-update.
    {
        let services = services_ctx.clone();
        let log_lines = log_lines.clone();
        use_effect_with((), move |()| {
            let services_for_cleanup = services.clone();
            let sub_id = services.event.subscribe(move |msg| match msg {
                EventMessage::PlaylistUpdateProgress(progress) => {
                    push_log_line(&log_lines, format!("{} [playlist] {}", format_hms_now(), progress.message));
                }
                EventMessage::LibraryScanProgress(progress) => {
                    push_log_line(
                        &log_lines,
                        format!("{} [library] {}", format_hms_now(), format_library_log_line(&progress.summary)),
                    );
                }
                _ => {}
            });
            move || {
                services_for_cleanup.event.unsubscribe(sub_id);
            }
        });
    }

    // Auto-scroll the log container to the bottom whenever a new line is appended.
    {
        let log_container_ref = log_container_ref.clone();
        let log_snapshot = log_lines.lines.clone();
        use_effect_with(log_snapshot, move |_| {
            if let Some(el) = log_container_ref.get().and_then(|n| n.dyn_into::<HtmlElement>().ok()) {
                el.set_scroll_top(el.scroll_height());
            }
            || ()
        });
    }

    let handle_all_select = {
        let selected_targets = selected_targets.clone();
        Callback::from(move |_| {
            selected_targets.clear();
        })
    };

    let handle_target_select = {
        let selected_targets = selected_targets.clone();
        Callback::from(move |target: Rc<ConfigTargetDto>| {
            let exists = selected_targets.current().iter().any(|t| t.id == target.id);
            if exists {
                selected_targets.retain(|t: &Rc<ConfigTargetDto>| t.id != target.id);
            } else {
                selected_targets.push(target);
            }
        })
    };

    let handle_update = {
        let translate = translate.clone();
        let services = services_ctx.clone();
        let selected_targets = selected_targets.clone();
        let log_lines = log_lines.clone();
        let updating = updating.clone();
        Callback::from(move |_| {
            if !can_write_playlist || *updating {
                return;
            }
            updating.set(true);
            log_lines.dispatch(LogLinesAction::Clear);
            let selected_targets = selected_targets.clone();
            let services = services.clone();
            let translate = translate.clone();
            let updating = updating.clone();
            spawn_local(async move {
                let target_names = {
                    let targets = selected_targets.current();
                    targets.iter().map(|t| t.name.clone()).collect::<Vec<String>>()
                };
                let update_target_names = target_names.iter().map(std::string::String::as_str).collect::<Vec<&str>>();
                if services.playlist.update_targets(&update_target_names).await {
                    services.toastr.success(translate.t("MESSAGES.PLAYLIST_UPDATE.SUCCESS"));
                } else {
                    services.toastr.error(translate.t("MESSAGES.PLAYLIST_UPDATE.FAIL"));
                }
                updating.set(false);
            });
        })
    };

    let handle_update_content = {
        let services = services_ctx.clone();
        let translate = translate.clone();
        let log_lines = log_lines.clone();
        let library_updating = library_updating.clone();
        Callback::from(move |name: String| {
            if !can_write_library || *library_updating {
                return;
            }
            let services = services.clone();
            let translate = translate.clone();
            let log_lines = log_lines.clone();
            let library_updating = library_updating.clone();
            wasm_bindgen_futures::spawn_local(async move {
                let mode = match name.as_str() {
                    ACTION_UPDATE_LIBRARY => 1,
                    ACTION_UPDATE_LIBRARY_FORCE => 2,
                    _ => 0,
                };
                if mode > 0 {
                    library_updating.set(true);
                    log_lines.dispatch(LogLinesAction::Clear);
                    match services.config.update_library(mode == 2).await {
                        Ok(()) => services.toastr.success(translate.t("MESSAGES.LIBRARY_UPDATE.SUCCESS")),
                        Err(_err) => services.toastr.error(translate.t("MESSAGES.LIBRARY_UPDATE.FAIL")),
                    }
                    library_updating.set(false);
                }
            });
        })
    };

    let library_enabled = config_ctx.config.as_ref().is_some_and(|c| c.config.is_library_enabled());
    let log_lines_render = {
        let log_lines = log_lines.clone();
        use_memo(log_lines.lines.clone(), |lines| {
            lines
                .iter()
                .map(|line| html! { <div class="tp__playlist-update-view__log-line">{ line }</div> })
                .collect::<Vec<Html>>()
        })
    };

    html! {
      <div class="tp__playlist-update-view">
         <Breadcrumbs items={&*breadcrumbs}/>
         <div class="tp__playlist-update-view__header">
          <h1>{ translate.t("LABEL.UPDATE")}</h1>
          <div class="tp__config-view__header-tools">
            {html_if!(can_write_library && library_enabled, {
                <div class="tp__radio-button-group">
                <TextButton class="tertiary" name={ACTION_UPDATE_LIBRARY}
                    icon="Refresh"
                    disabled={*library_updating}
                    title={ translate.t(LABEL_UPDATE_LOCAL_LIBRARY)}
                    onclick={handle_update_content.clone()}></TextButton>
                <TextButton class="tertiary" name={ACTION_UPDATE_LIBRARY_FORCE}
                    title={ translate.t(LABEL_FORCE)}
                    disabled={*library_updating}
                    onclick={handle_update_content.clone()}></TextButton>
                </div>
            })}
            </div>
        { html_if!(can_write_playlist, {
            <TextButton class="primary" name="playlist_update"
                   icon="Refresh"
                   disabled={*updating}
                   title={ translate.t("LABEL.UPDATE")}
                   onclick={handle_update}></TextButton>
        })}
        </div>
        <Card>
         <div class="tp__playlist-update-view__body">
         {
            if let Some(data) = playlist_ctx.sources.as_ref().as_ref().filter(|data| data.iter().any(|(_, targets)| !targets.is_empty())) {
              html! {
                <>
                <TextButton class={if selected_targets.current().is_empty() { "active" } else {""}}
                    name={translate.t("LABEL.ALL")} title={translate.t("LABEL.ALL")} icon={"SelectAll"}
                    onclick={handle_all_select}/>
                {
                  data.iter().flat_map(|(_inputs, targets)| targets)
                    .map(Rc::clone)
                    .map(|target| {
                        let handle_click = handle_target_select.clone();
                        let target_name = target.name.clone();
                        let button_class = if selected_targets.current().iter().any(|t| t.id == target.id) { "active" } else {""};
                        html! {
                          <TextButton class={button_class}
                            name={target_name.clone()} title={target_name} icon={"UpdateChecked"}
                             onclick={move |_| handle_click.emit(target.clone())}/>
                        }
                  }).collect::<Html>()
                }
                </>
              }
            } else {
              html! { <NoContent text={translate.t("MESSAGES.PLAYLIST_UPDATE.NO_TARGETS")}/> }
            }
         }
         </div>
         </Card>
         <div class="tp__playlist-update-view__log" ref={log_container_ref}>
            { for log_lines_render.iter().cloned() }
         </div>
      </div>
    }
}

#[cfg(test)]
mod tests {
    use super::{append_log_line_entries, format_library_log_line, MAX_LOG_LINES};
    use shared::model::{LibraryScanSummary, LibraryScanSummaryStatus};
    use yew::AttrValue;

    #[test]
    fn append_log_line_entries_keeps_latest_entries_when_log_is_capped() {
        let current = (0..MAX_LOG_LINES).map(|idx| AttrValue::from(format!("line-{idx}"))).collect::<Vec<_>>();

        let updated = append_log_line_entries(&current, "line-new".to_string());

        assert_eq!(updated.len(), MAX_LOG_LINES);
        assert_eq!(updated.first().map(AttrValue::as_str), Some("line-1"));
        assert_eq!(updated.last().map(AttrValue::as_str), Some("line-new"));
    }

    #[test]
    fn format_library_log_line_uses_human_readable_message() {
        let summary = LibraryScanSummary {
            status: LibraryScanSummaryStatus::Success,
            message: "Scan completed".to_string(),
            result: None,
        };

        assert_eq!(format_library_log_line(&summary), "Scan completed");
    }
}
