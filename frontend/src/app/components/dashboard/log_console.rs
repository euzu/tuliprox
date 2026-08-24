use crate::{
    app::components::{input::Input, RadioButtonGroup, TextButton},
    hooks::{use_clipboard_copy, use_log_stream, UseLogStreamOptions},
    i18n::use_translation,
};
use shared::model::{LogEntry, LogLevel};
use std::rc::Rc;
use web_sys::HtmlElement;
use yew::prelude::*;

fn filter_level_from_selection(selections: &[String]) -> Option<Option<LogLevel>> {
    let selection = selections.first()?;
    if selection.eq_ignore_ascii_case("ALL") {
        Some(None)
    } else {
        selection.parse().ok().map(Some)
    }
}

#[derive(Properties, Clone, PartialEq, Debug)]
pub struct LogConsoleProps {
    #[prop_or(true)]
    pub active: bool,
}

#[component]
pub fn LogConsole(props: &LogConsoleProps) -> Html {
    let translate = use_translation();
    let copy_clipboard = use_clipboard_copy();

    let auto_scroll = use_state(|| true);
    let search_query = use_state(String::new);
    let selected_level = use_state(|| None::<LogLevel>);

    let log_stream = use_log_stream(UseLogStreamOptions { active: props.active, max_lines: 2000 });

    let console_ref = use_node_ref();

    // Auto-scroll effect
    {
        let console_ref = console_ref.clone();
        let auto_scroll = *auto_scroll;
        let logs_count = log_stream.logs.len();

        use_effect_with((logs_count, auto_scroll), move |(_, auto_scroll)| {
            if *auto_scroll {
                if let Some(el) = console_ref.cast::<HtmlElement>() {
                    el.set_scroll_top(el.scroll_height());
                }
            }
            || ()
        });
    }

    // Scroll listener to detect if user manually scrolled up
    let on_scroll = {
        let console_ref = console_ref.clone();
        let auto_scroll = auto_scroll.clone();
        Callback::from(move |_| {
            if let Some(el) = console_ref.cast::<HtmlElement>() {
                let scroll_bottom = el.scroll_top() + el.client_height();
                let threshold = 35;
                let at_bottom = scroll_bottom >= el.scroll_height() - threshold;
                if at_bottom != *auto_scroll {
                    auto_scroll.set(at_bottom);
                }
            }
        })
    };

    // Filter logs based on search query and selected log level
    let filtered_logs: Vec<&LogEntry> = {
        let query = search_query.to_lowercase();
        let level_filter = *selected_level;

        log_stream
            .logs
            .iter()
            .filter(|entry| {
                if let Some(lvl) = level_filter {
                    if entry.level != lvl {
                        return false;
                    }
                }
                if !query.is_empty() {
                    let matches_msg = entry.message.to_lowercase().contains(&query);
                    let matches_target = entry.target.to_lowercase().contains(&query);
                    let matches_ts = entry.timestamp.to_lowercase().contains(&query);
                    let matches_lvl = entry.level.as_str().contains(&query);
                    if !matches_msg && !matches_target && !matches_ts && !matches_lvl {
                        return false;
                    }
                }
                true
            })
            .collect()
    };

    let on_select_level = {
        let selected_level = selected_level.clone();
        Callback::from(move |selections: Rc<Vec<String>>| {
            if let Some(level) = filter_level_from_selection(&selections) {
                selected_level.set(level);
            }
        })
    };

    // Search input callback
    let on_search_input = {
        let search_query = search_query.clone();
        Callback::from(move |value: String| {
            search_query.set(value);
        })
    };

    // Auto-scroll toggle callback
    let on_toggle_auto_scroll = {
        let auto_scroll = auto_scroll.clone();
        let console_ref = console_ref.clone();
        Callback::from(move |_| {
            let next = !*auto_scroll;
            auto_scroll.set(next);
            if next {
                if let Some(el) = console_ref.cast::<HtmlElement>() {
                    el.set_scroll_top(el.scroll_height());
                }
            }
        })
    };

    // Clear logs callback
    let on_clear = {
        let clear = log_stream.clear.clone();
        Callback::from(move |_| {
            clear.emit(());
        })
    };

    // Copy logs callback
    let on_copy = {
        let copy_clipboard = copy_clipboard.clone();
        let formatted_text: String = filtered_logs
            .iter()
            .map(|e| format!("{} [{:>5}] [{}] {}", e.timestamp, e.level.as_str().to_uppercase(), e.target, e.message))
            .collect::<Vec<_>>()
            .join("\n");

        Callback::from(move |_| {
            copy_clipboard.emit(formatted_text.clone());
        })
    };

    let level_options =
        Rc::new(vec!["ALL", "ERROR", "WARN", "INFO", "DEBUG", "TRACE"].into_iter().map(String::from).collect());
    let level_selection = Rc::new(vec![(*selected_level).as_ref().map_or("ALL", LogLevel::as_str).to_uppercase()]);

    html! {
        <div class="tp__log-console">
            <div class="tp__log-console__toolbar">
                <div class="tp__log-console__filters">
                    <RadioButtonGroup
                        multi_select={false}
                        none_allowed={false}
                        options={level_options}
                        selected={level_selection}
                        on_select={on_select_level}
                    />
                </div>

                <div class="tp__log-console__controls">
                    <div class="tp__log-console__search">
                        <Input
                            placeholder={translate.t("LABEL.SEARCH_LOGS")}
                            value={(*search_query).clone()}
                            on_change={on_search_input}
                        />
                    </div>

                    <div class="tp__log-console__actions">
                        <TextButton
                            name="autoscroll_log"
                            icon={if *auto_scroll { "Pause" } else { "Play" }}
                            class={format!("tp__log-console__action-btn {}",  if *auto_scroll { "tp__log-console__action-btn--active" } else { "" })}
                            title={translate.t("LABEL.AUTO_SCROLL")}
                            hint={if *auto_scroll { translate.t("LABEL.PAUSE_SCROLL") } else { translate.t("LABEL.RESUME_SCROLL") }}
                            onclick={on_toggle_auto_scroll}
                        />

                        <TextButton
                            name="copy_log"
                            icon="Copy"
                            class="tp__log-console__action-btn"
                            hint={translate.t("LABEL.COPY_LOGS")}
                            title={ translate.t("LABEL.COPY_LOGS") }
                            onclick={on_copy}
                        />

                        <TextButton
                            name="delete_log"
                            icon="Delete"
                            class="tp__log-console__action-btn"
                            title={ translate.t("LABEL.CLEAR_LOGS") }
                            hint={translate.t("LABEL.CLEAR_LOGS")}
                            onclick={on_clear}
                        />
                    </div>

                    <div class="tp__log-console__status">
                        <span class={classes!(
                            "tp__log-console__status-dot",
                            if log_stream.connected { "tp__log-console__status-dot--connected" } else { "tp__log-console__status-dot--disconnected" }
                        )} />
                    </div>
                </div>
            </div>

            <div class="tp__log-console__output" ref={console_ref} onscroll={on_scroll}>
                {
                    if filtered_logs.is_empty() {
                        html! {
                            <div class="tp__log-console__empty">
                                { translate.t("LABEL.NO_LOGS") }
                            </div>
                        }
                    } else {
                        html! {
                            <>
                                {
                                    for filtered_logs.iter().enumerate().map(|(idx, entry)| {
                                        let level_str = entry.level.as_str();
                                        let level_cls = format!("tp__log-console__level--{}", level_str);
                                        let msg_cls = match entry.level {
                                            LogLevel::Error => "tp__log-console__message--error",
                                            LogLevel::Warn => "tp__log-console__message--warn",
                                            _ => "",
                                        };

                                        html! {
                                            <div class="tp__log-console__row" key={format!("{}-{}-{}", idx, entry.timestamp, entry.message)}>
                                                <span class="tp__log-console__timestamp">{ &entry.timestamp }</span>
                                                <span class={classes!("tp__log-console__level", level_cls)}>
                                                    { level_str }
                                                </span>
                                                <span class="tp__log-console__target">{ &entry.target }</span>
                                                <span class={classes!("tp__log-console__message", msg_cls)}>
                                                    { &entry.message }
                                                </span>
                                            </div>
                                        }
                                    })
                                }
                            </>
                        }
                    }
                }
            </div>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_selection_maps_all_and_log_levels() {
        assert_eq!(filter_level_from_selection(&["ALL".to_string()]), Some(None));

        for (value, expected) in [
            ("ERROR", LogLevel::Error),
            ("WARN", LogLevel::Warn),
            ("INFO", LogLevel::Info),
            ("DEBUG", LogLevel::Debug),
            ("TRACE", LogLevel::Trace),
        ] {
            assert_eq!(filter_level_from_selection(&[value.to_string()]), Some(Some(expected)));
        }
    }

    #[test]
    fn filter_selection_ignores_empty_and_invalid_values() {
        assert_eq!(filter_level_from_selection(&[]), None);
        assert_eq!(filter_level_from_selection(&["INVALID".to_string()]), None);
    }
}
