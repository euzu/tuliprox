use std::collections::HashMap;
use std::rc::Rc;
use web_sys::HtmlInputElement;
use yew::prelude::*;
use crate::app::components::chip::Chip;

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
    #[prop_or_else(|| "Add key".to_string())]
    pub key_placeholder: String,
    #[prop_or_else(|| "Add value".to_string())]
    pub value_placeholder: String,
}

#[function_component]
pub fn KeyValueEditor(props: &KeyValueEditorProps) -> Html {
    let KeyValueEditorProps {
        label,
        entries,
        on_change,
        readonly,
        key_placeholder,
        value_placeholder,
    } = props.clone();

    // local state for editing
    let entry_state = use_state(|| {
        entries.iter().map(|(k,v)| Rc::new(KeyValue { key: k.clone(), value: v.clone() })).collect::<Vec<_>>()
    });
    let new_key = use_state(String::default);
    let new_value = use_state(String::default);

    // keep local state in sync when parent updates
    {
        let entry_state = entry_state.clone();
        use_effect_with(entries.clone(), move |entries| {
            entry_state.set(
                entries.iter()
                    .map(|(k,v)| Rc::new(KeyValue { key: k.clone(), value: v.clone() }))
                    .collect()
            );
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
            let map = updated.iter().map(|kv| (kv.key.clone(), kv.value.clone())).collect::<HashMap<_,_>>();
            on_change.emit(map.clone());
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

    // add new entry on enter in value field
    let on_keydown_value = {
        let new_key = new_key.clone();
        let new_value = new_value.clone();
        let entry_state = entry_state.clone();
        let on_change = on_change.clone();
        Callback::from(move |e: KeyboardEvent| {
            if e.key() == "Enter" {
                e.prevent_default();
                e.stop_propagation();
                let key = (*new_key).trim().to_string();
                let value = (*new_value).trim().to_string();
                if !key.is_empty() && !value.is_empty() && !entry_state.iter().any(|kv| kv.key == key) {
                    let mut updated = (*entry_state).clone();
                    updated.push(Rc::new(KeyValue { key: key.clone(), value: value.clone() }));
                    // emit new HashMap
                    let map = updated.iter().map(|kv| (kv.key.clone(), kv.value.clone())).collect::<HashMap<_,_>>();
                    on_change.emit(map.clone());
                    entry_state.set(updated);
                }
                new_key.set(String::new());
                new_value.set(String::new());
            }
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
                      </div>
                    }
                }
            }
        </div>
    }
}
