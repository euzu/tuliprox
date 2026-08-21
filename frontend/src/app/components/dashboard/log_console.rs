use crate::{
    app::components::AppIcon,
    hooks::{use_clipboard_copy, use_log_stream, UseLogStreamOptions},
    i18n::use_translation,
};
use shared::model::{LogEntry, LogLevel};
use wasm_bindgen::JsCast;
use web_sys::{HtmlElement, HtmlInputElement};
use yew::prelude::*;

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

    // Level selector callback
    let on_select_level = |level: Option<LogLevel>| {
        let selected_level = selected_level.clone();
        Callback::from(move |_| {
            selected_level.set(level);
        })
    };

    // Search input callback
    let on_search_input = {
        let search_query = search_query.clone();
        Callback::from(move |e: InputEvent| {
            if let Some(input) = e.target().and_then(|t| t.dyn_into::<HtmlInputElement>().ok()) {
                search_query.set(input.value());
            }
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

    let levels = [
        (None, "ALL", "tp__log-console__filter-btn--all"),
        (Some(LogLevel::Error), "ERROR", "tp__log-console__filter-btn--error"),
        (Some(LogLevel::Warn), "WARN", "tp__log-console__filter-btn--warn"),
        (Some(LogLevel::Info), "INFO", "tp__log-console__filter-btn--info"),
        (Some(LogLevel::Debug), "DEBUG", "tp__log-console__filter-btn--debug"),
        (Some(LogLevel::Trace), "TRACE", "tp__log-console__filter-btn--trace"),
    ];

    html! {
        <div class="tp__log-console">
            <div class="tp__log-console__toolbar">
                <div class="tp__log-console__filters">
                    {
                        for levels.iter().map(|(lvl, label, cls)| {
                            let is_active = *selected_level == *lvl;
                            let active_cls = if is_active { "tp__log-console__filter-btn--active" } else { "" };
                            html! {
                                <button
                                    type="button"
                                    class={classes!("tp__log-console__filter-btn", *cls, active_cls)}
                                    onclick={on_select_level(*lvl)}
                                >
                                    { *label }
                                </button>
                            }
                        })
                    }
                </div>

                <div class="tp__log-console__controls">
                    <div class="tp__log-console__search">
                        <input
                            type="text"
                            placeholder={translate.t("LABEL.SEARCH_LOGS")}
                            value={(*search_query).clone()}
                            oninput={on_search_input}
                        />
                    </div>

                    <div class="tp__log-console__actions">
                        <button
                            type="button"
                            class={classes!("tp__log-console__action-btn", if *auto_scroll { "tp__log-console__action-btn--active" } else { "" })}
                            title={if *auto_scroll { translate.t("LABEL.PAUSE_SCROLL") } else { translate.t("LABEL.RESUME_SCROLL") }}
                            onclick={on_toggle_auto_scroll}
                        >
                            <AppIcon name={if *auto_scroll { "Pause" } else { "Play" }} />
                            <span>{ translate.t("LABEL.AUTO_SCROLL") }</span>
                        </button>

                        <button
                            type="button"
                            class="tp__log-console__action-btn"
                            title={translate.t("LABEL.COPY_LOGS")}
                            onclick={on_copy}
                        >
                            <AppIcon name="Copy" />
                            <span>{ translate.t("LABEL.COPY_LOGS") }</span>
                        </button>

                        <button
                            type="button"
                            class="tp__log-console__action-btn"
                            title={translate.t("LABEL.CLEAR_LOGS")}
                            onclick={on_clear}
                        >
                            <AppIcon name="Delete" />
                            <span>{ translate.t("LABEL.CLEAR_LOGS") }</span>
                        </button>
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
