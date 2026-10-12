//! Editor for a target's `watch` entries: a pattern plus the contexts it
//! reports (`channels`, `group_added`, `group_removed`).

use crate::{
    app::components::{Chip, IconButton, RadioButtonGroup},
    i18n::use_translation,
};
use shared::model::{ConfigWatchDto, Prepare, WatchContext, WatchContextSet};
use std::rc::Rc;
use web_sys::HtmlInputElement;
use yew::prelude::*;

/// Context, its config spelling (the selection value), and its label key.
const CONTEXTS: [(WatchContext, &str, &str); 3] = [
    (WatchContext::Channels, "channels", "LABEL.WATCH_CHANNELS"),
    (WatchContext::GroupAdded, "group_added", "LABEL.WATCH_GROUP_ADDED"),
    (WatchContext::GroupRemoved, "group_removed", "LABEL.WATCH_GROUP_REMOVED"),
];

/// The selection the context picker shows for `on`.
fn context_names(on: WatchContextSet) -> Vec<String> {
    CONTEXTS.iter().filter(|(context, _, _)| on.contains(*context)).map(|(_, name, _)| (*name).to_string()).collect()
}

/// The contexts a picker selection stands for.
fn contexts_from_names(names: &[String]) -> WatchContextSet {
    let mut on = WatchContextSet::new();
    for (context, name, _) in CONTEXTS {
        if names.iter().any(|selected| selected == name) {
            on.set(context);
        }
    }
    on
}

/// What a fresh input row starts with: the short form.
fn default_contexts() -> WatchContextSet { ConfigWatchDto::channels("").on }

#[derive(Properties, Clone, PartialEq)]
pub struct WatchEntriesEditorProps {
    pub entries: Rc<Vec<ConfigWatchDto>>,
    #[prop_or_else(Callback::noop)]
    pub on_change: Callback<Vec<ConfigWatchDto>>,
    #[prop_or(true)]
    pub readonly: bool,
}

#[derive(Debug, PartialEq)]
enum CandidateDecision {
    Accepted(ConfigWatchDto),
    /// Blank, duplicate or without a context: nothing to add, nothing to say.
    Rejected,
    /// The entry would fail config validation; the message says why.
    Invalid(String),
}

/// An entry with no context or a blank or duplicate pattern is never added,
/// so the editor cannot produce a config that `prepare` rejects.
fn candidate(pattern: &str, on: WatchContextSet, entries: &[ConfigWatchDto]) -> CandidateDecision {
    let pattern = pattern.trim();
    if pattern.is_empty() || on.is_empty() || entries.iter().any(|entry| entry.pattern == pattern) {
        return CandidateDecision::Rejected;
    }
    let mut entry = ConfigWatchDto { pattern: pattern.to_string(), on };
    match entry.prepare(()) {
        Ok(()) => CandidateDecision::Accepted(entry),
        Err(err) => CandidateDecision::Invalid(err.to_string()),
    }
}

/// Append `pattern` for the short form, `pattern [context, ...]` otherwise.
pub fn push_watch_entry_label(out: &mut String, entry: &ConfigWatchDto, translate: impl Fn(&str) -> String) {
    out.push_str(&entry.pattern);
    if entry.on == default_contexts() {
        return;
    }
    let mut separator = " [";
    for (context, _, key) in CONTEXTS {
        if entry.has(context) {
            out.push_str(separator);
            out.push_str(&translate(key));
            separator = ", ";
        }
    }
    out.push(']');
}

/// [`push_watch_entry_label`] into a fresh `String`.
pub fn watch_entry_label(entry: &ConfigWatchDto, translate: impl Fn(&str) -> String) -> String {
    let mut label = String::new();
    push_watch_entry_label(&mut label, entry, translate);
    label
}

#[component]
pub fn WatchEntriesEditor(props: &WatchEntriesEditorProps) -> Html {
    // Shared with the callbacks by reference count; a list is only copied
    // when an edit actually produces a new one.
    let entries = Rc::clone(&props.entries);
    let on_change = props.on_change.clone();
    let readonly = props.readonly;
    let translate = use_translation();
    let pattern = use_state(String::new);
    let on = use_state(default_contexts);
    // Why the last add was refused; cleared as soon as the pattern changes.
    let error = use_state(|| None::<String>);

    let on_remove = {
        let entries = Rc::clone(&entries);
        let on_change = on_change.clone();
        Callback::from(move |removed: usize| {
            let mut updated = entries.as_ref().clone();
            if removed < updated.len() {
                updated.remove(removed);
                on_change.emit(updated);
            }
        })
    };

    let on_input = {
        let pattern = pattern.clone();
        let error = error.clone();
        Callback::from(move |e: InputEvent| {
            let input: HtmlInputElement = e.target_unchecked_into();
            pattern.set(input.value());
            if error.is_some() {
                error.set(None);
            }
        })
    };

    let add = {
        let entries = Rc::clone(&entries);
        let pattern = pattern.clone();
        let on = on.clone();
        let error = error.clone();
        Callback::from(move |()| match candidate(&pattern, *on, &entries) {
            CandidateDecision::Accepted(entry) => {
                let mut updated = entries.as_ref().clone();
                updated.push(entry);
                on_change.emit(updated);
                pattern.set(String::new());
                on.set(default_contexts());
            }
            CandidateDecision::Invalid(message) => error.set(Some(message)),
            CandidateDecision::Rejected => {}
        })
    };

    let on_keydown = {
        let add = add.clone();
        Callback::from(move |e: KeyboardEvent| {
            if e.key() == "Enter" {
                e.prevent_default();
                e.stop_propagation();
                add.emit(());
            }
        })
    };

    // At least one context stays selected: the group refuses to drop the
    // last one, so an entry with an empty `on` cannot be built here.
    let context_options = use_memo((), |()| CONTEXTS.iter().map(|(_, name, _)| (*name).to_string()).collect());
    let context_labels = Rc::new(CONTEXTS.iter().map(|(_, _, key)| translate.t(key)).collect::<Vec<_>>());
    let on_select_contexts = {
        let on = on.clone();
        Callback::from(move |names: Rc<Vec<String>>| on.set(contexts_from_names(&names)))
    };

    let on_add_click = Callback::from(move |(_name, e): (String, MouseEvent)| {
        e.prevent_default();
        e.stop_propagation();
        add.emit(());
    });

    html! {
        <div class="tp__watch-entries-editor">
            if !readonly {
                // Fixed input area: the list below scrolls, this stays put.
                <div class="tp__watch-entries-editor__form">
                    <div class="tp__watch-entries-editor__pattern">
                        <div class={classes!("tp__input", error.is_some().then_some("invalid"))}>
                            <div class="tp__input-wrapper">
                                <input
                                    type="text"
                                    value={(*pattern).clone()}
                                    oninput={on_input}
                                    onkeydown={on_keydown}
                                    placeholder={translate.t("LABEL.ADD_WATCH")}
                                />
                            </div>
                        </div>
                        <IconButton name={"AddWatch"} icon="Add" onclick={on_add_click} />
                    </div>
                    <RadioButtonGroup
                        multi_select={true}
                        none_allowed={false}
                        options={context_options}
                        labels={Some(context_labels)}
                        selected={Rc::new(context_names(*on))}
                        on_select={on_select_contexts}
                    />
                    if let Some(message) = (*error).as_ref() {
                        <span class="tp__watch-entries-editor__error">{ message }</span>
                    }
                </div>
            }
            <div class="tp__watch-entries-editor__entries">
            { for entries.iter().enumerate().map(|(index, entry)| {
                html! {
                    <Chip
                        label={watch_entry_label(entry, |key| translate.t(key))}
                        removable={!readonly}
                        on_remove={if readonly { Callback::noop() } else { on_remove.reform(move |_| index) }}
                    />
                }
            })}
            </div>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::{candidate, context_names, contexts_from_names, watch_entry_label, CandidateDecision};
    use shared::model::{ConfigWatchDto, WatchContext, WatchContextSet};

    fn set(contexts: &[WatchContext]) -> WatchContextSet { ConfigWatchDto::new("", contexts.iter().copied()).on }

    #[test]
    fn blank_pattern_is_rejected() {
        assert_eq!(candidate("  ", set(&[WatchContext::Channels]), &[]), CandidateDecision::Rejected);
    }

    #[test]
    fn no_context_is_rejected() {
        assert_eq!(candidate("^A", WatchContextSet::new(), &[]), CandidateDecision::Rejected);
    }

    #[test]
    fn duplicate_pattern_is_rejected() {
        let existing = [ConfigWatchDto::channels("^A")];
        assert_eq!(candidate(" ^A ", set(&[WatchContext::GroupAdded]), &existing), CandidateDecision::Rejected);
    }

    /// An invalid regex used to be added and only fail on save; the editor
    /// now rejects it with the same message config load would give.
    #[test]
    fn invalid_regex_is_reported() {
        match candidate("^(Sport", set(&[WatchContext::Channels]), &[]) {
            CandidateDecision::Invalid(message) => {
                assert!(message.contains("Invalid watch regular expression"), "{message}");
            }
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    #[test]
    fn valid_candidate_is_trimmed() {
        assert_eq!(
            candidate(" ^A ", set(&[WatchContext::GroupAdded]), &[]),
            CandidateDecision::Accepted(ConfigWatchDto::new("^A", [WatchContext::GroupAdded]))
        );
    }

    #[test]
    fn selection_round_trips_through_the_contexts_set() {
        let on = set(&[WatchContext::GroupRemoved, WatchContext::Channels]);
        let names = context_names(on);
        assert_eq!(names.as_slice(), ["channels", "group_removed"]);
        assert_eq!(contexts_from_names(&names), on);
    }

    #[test]
    fn label_shows_contexts_only_for_object_form() {
        let t = |key: &str| key.rsplit('.').next().unwrap_or_default().to_lowercase();
        assert_eq!(watch_entry_label(&ConfigWatchDto::channels("^A"), t), "^A");
        assert_eq!(
            watch_entry_label(&ConfigWatchDto::new("^B", [WatchContext::GroupAdded, WatchContext::Channels]), t),
            "^B [watch_channels, watch_group_added]"
        );
    }
}
