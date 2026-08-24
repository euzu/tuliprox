use crate::{
    app::components::{chip::Chip, IconButton},
    i18n::use_translation,
};
use std::{collections::HashMap, rc::Rc};
use web_sys::HtmlInputElement;
use yew::prelude::*;

#[derive(Clone, PartialEq, Debug)]
pub struct KeyValue {
    pub key: String,
    pub value: String,
}

#[derive(Properties, Clone, PartialEq)]
pub struct KeyValueEditorProps {
    #[prop_or_default]
    pub label: Option<String>,
    pub entries: HashMap<String, String>,
    #[prop_or_else(Callback::noop)]
    pub on_change: Callback<HashMap<String, String>>,
    #[prop_or(true)]
    pub readonly: bool,
    #[prop_or_default]
    pub key_placeholder: String,
    #[prop_or_default]
    pub value_placeholder: String,
    #[prop_or_default]
    pub validate_entry: Option<Callback<(String, String), bool>>,
}

#[derive(Debug, PartialEq)]
enum CandidateDecision {
    Accepted(KeyValue),
    RejectedClear,
    RejectedPreserve,
}

fn candidate_entry(
    key: &str,
    value: &str,
    entries: &[Rc<KeyValue>],
    validate_entry: Option<&Callback<(String, String), bool>>,
) -> CandidateDecision {
    let key = key.trim();
    let value = value.trim();
    if key.is_empty() || value.is_empty() || entries.iter().any(|entry| entry.key == key) {
        return CandidateDecision::RejectedClear;
    }

    let candidate = KeyValue { key: key.to_owned(), value: value.to_owned() };
    if validate_entry.is_some_and(|validator| !validator.emit((candidate.key.clone(), candidate.value.clone()))) {
        return CandidateDecision::RejectedPreserve;
    }
    CandidateDecision::Accepted(candidate)
}

#[component]
pub fn KeyValueEditor(props: &KeyValueEditorProps) -> Html {
    let KeyValueEditorProps { label, entries, on_change, readonly, key_placeholder, value_placeholder, validate_entry } =
        props.clone();
    let translate = use_translation();
    // Empty placeholder props fall back to localized defaults
    let key_placeholder = if key_placeholder.is_empty() { translate.t("LABEL.ADD_KEY") } else { key_placeholder };
    let value_placeholder =
        if value_placeholder.is_empty() { translate.t("LABEL.ADD_VALUE") } else { value_placeholder };

    // local state for editing
    let entry_state = use_state(|| {
        entries.iter().map(|(k, v)| Rc::new(KeyValue { key: k.clone(), value: v.clone() })).collect::<Vec<_>>()
    });
    let new_key = use_state(String::default);
    let new_value = use_state(String::default);

    // keep local state in sync when parent updates
    {
        let entry_state = entry_state.clone();
        use_effect_with(entries.clone(), move |entries| {
            entry_state
                .set(entries.iter().map(|(k, v)| Rc::new(KeyValue { key: k.clone(), value: v.clone() })).collect());
            || ()
        });
    }

    // remove existing entry
    let on_remove = {
        let entry_state = entry_state.clone();
        let on_change = on_change.clone();
        Callback::from(move |key: String| {
            let mut updated = (*entry_state).clone();
            updated.retain(|kv| kv.key != key);
            // emit new HashMap
            let map = updated.iter().map(|kv| (kv.key.clone(), kv.value.clone())).collect::<HashMap<_, _>>();
            on_change.emit(map);
            entry_state.set(updated);
        })
    };

    // input change for new key/value
    let on_input_key = {
        let new_key = new_key.clone();
        Callback::from(move |e: InputEvent| {
            let input: HtmlInputElement = e.target_unchecked_into();
            new_key.set(input.value());
        })
    };

    let on_input_value = {
        let new_value = new_value.clone();
        Callback::from(move |e: InputEvent| {
            let input: HtmlInputElement = e.target_unchecked_into();
            new_value.set(input.value());
        })
    };

    let add_key_value = {
        let new_key = new_key.clone();
        let new_value = new_value.clone();
        let entry_state = entry_state.clone();
        let on_change = on_change.clone();
        Callback::from(move |()| {
            match candidate_entry(&new_key, &new_value, (*entry_state).as_slice(), validate_entry.as_ref()) {
                CandidateDecision::Accepted(candidate) => {
                    let mut updated = (*entry_state).clone();
                    updated.push(Rc::new(candidate));
                    // emit new HashMap
                    let map = updated.iter().map(|kv| (kv.key.clone(), kv.value.clone())).collect::<HashMap<_, _>>();
                    on_change.emit(map);
                    entry_state.set(updated);
                    new_key.set(String::new());
                    new_value.set(String::new());
                }
                CandidateDecision::RejectedClear => {
                    new_key.set(String::new());
                    new_value.set(String::new());
                }
                CandidateDecision::RejectedPreserve => {}
            }
        })
    };

    // add new entry on enter in value field
    let on_keydown_value = {
        let add_key_value = add_key_value.clone();
        Callback::from(move |e: KeyboardEvent| {
            if e.key() == "Enter" {
                e.prevent_default();
                e.stop_propagation();
                add_key_value.emit(());
            }
        })
    };

    let on_add_value = {
        let add_key_value = add_key_value.clone();
        Callback::from(move |(_name, e): (String, MouseEvent)| {
            e.prevent_default();
            e.stop_propagation();
            add_key_value.emit(());
        })
    };

    html! {
        <div class="tp__keyvalue-editor">
            { if let Some(lbl) = &label {
                html! { <label>{ lbl }</label> }
            } else { html!{} } }
            <div class="tp__keyvalue-editor__entries">
            { for (*entry_state).iter().map(|kv| {
                let key_clone = kv.key.clone();
                html! {
                    <Chip
                        label={format!("{}: {}", kv.key, kv.value)}
                        removable={!readonly}
                        on_remove={if readonly { Callback::noop() } else { on_remove.reform(move |_| key_clone.clone()) }}
                    />
                }
            })}
           </div>
            {
                if readonly {
                    html! {}
                } else {
                    html! {
                      <div class="tp__keyvalue-editor__inputs">
                        <div class="tp__input">
                        <div class=" tp__input-wrapper">
                            <input
                                type="text"
                                value={(*new_key).clone()}
                                oninput={on_input_key}
                                placeholder={key_placeholder.clone()}
                            />
                        </div>
                        </div>
                        <div class="tp__input">
                        <div class=" tp__input-wrapper">
                            <input
                                type="text"
                                value={(*new_value).clone()}
                                oninput={on_input_value}
                                onkeydown={on_keydown_value}
                                placeholder={value_placeholder.clone()}
                            />
                        </div>
                        </div>
                        <IconButton
                            name={"AddKeyValue"}
                            icon="Add"
                            onclick={on_add_value.clone()}
                        />
                      </div>
                    }
                }
            }
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn candidate_trims_before_validation() {
        let validator = Callback::from(|(key, value): (String, String)| key == "key" && value == "value");

        let candidate = candidate_entry("  key  ", "  value  ", &[], Some(&validator));

        assert_eq!(
            candidate,
            CandidateDecision::Accepted(KeyValue { key: "key".to_owned(), value: "value".to_owned() })
        );
    }

    #[test]
    fn blank_candidate_does_not_invoke_validator() {
        let calls = Rc::new(Cell::new(0));
        let validator_calls = calls.clone();
        let validator = Callback::from(move |_: (String, String)| {
            validator_calls.set(validator_calls.get() + 1);
            true
        });

        assert_eq!(candidate_entry("  ", "value", &[], Some(&validator)), CandidateDecision::RejectedClear);
        assert_eq!(calls.get(), 0);
    }

    #[test]
    fn duplicate_candidate_does_not_invoke_validator() {
        let entries = vec![Rc::new(KeyValue { key: "key".to_owned(), value: "old".to_owned() })];
        let calls = Rc::new(Cell::new(0));
        let validator_calls = calls.clone();
        let validator = Callback::from(move |_: (String, String)| {
            validator_calls.set(validator_calls.get() + 1);
            true
        });

        assert_eq!(candidate_entry(" key ", "new", &entries, Some(&validator)), CandidateDecision::RejectedClear);
        assert_eq!(calls.get(), 0);
    }

    #[test]
    fn rejected_candidate_preserves_inputs_and_entries_then_corrected_candidate_succeeds() {
        let key = " key ".to_owned();
        let value = " rejected ".to_owned();
        let entries = vec![Rc::new(KeyValue { key: "existing".to_owned(), value: "value".to_owned() })];
        let original_entries = entries.clone();
        let validator = Callback::from(|(_, value): (String, String)| value == "accepted");

        assert_eq!(candidate_entry(&key, &value, &entries, Some(&validator)), CandidateDecision::RejectedPreserve);
        assert_eq!(key, " key ");
        assert_eq!(value, " rejected ");
        assert_eq!(entries, original_entries);
        assert_eq!(
            candidate_entry(&key, " accepted ", &entries, Some(&validator)),
            CandidateDecision::Accepted(KeyValue { key: "key".to_owned(), value: "accepted".to_owned() })
        );
    }

    #[test]
    fn candidate_is_accepted_without_validator() {
        assert_eq!(
            candidate_entry("key", "value", &[], None),
            CandidateDecision::Accepted(KeyValue { key: "key".to_owned(), value: "value".to_owned() })
        );
    }

    #[test]
    fn structural_rejection_preserves_legacy_input_clearing() {
        assert_eq!(candidate_entry(" ", "value", &[], None), CandidateDecision::RejectedClear);
        let entries = vec![Rc::new(KeyValue { key: "key".to_owned(), value: "old".to_owned() })];
        assert_eq!(candidate_entry("key", "new", &entries, None), CandidateDecision::RejectedClear);
    }
}
