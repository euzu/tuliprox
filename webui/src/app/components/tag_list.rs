use web_sys::HtmlInputElement;
use yew::prelude::*;
use std::rc::Rc;
use crate::app::components::chip::{Chip};

#[derive(Clone, PartialEq, Debug)]
pub struct Tag {
    pub label: String,
    pub class: Option<String>,
}

#[derive(Properties, Clone, PartialEq)]
pub struct TagListProps {
    pub tags: Vec<Rc<Tag>>,
    #[prop_or_else(Callback::noop)]
    pub on_change: Callback<Vec<Rc<Tag>>>,
    #[prop_or(false)]
    pub removable: bool,
    #[prop_or(false)]
    pub allow_add: bool,
}

#[function_component(TagList)]
pub fn tag_list(props: &TagListProps) -> Html {
    let TagListProps {
        tags,
        on_change,
        removable,
        allow_add,
    } = props.clone();

    let tag_state = use_state(|| tags.clone());
    let new_tag = use_state(String::default);

    let on_remove = {
        let tag_state = tag_state.clone();
        let on_change = on_change.clone();
        Callback::from(move |tag_label: String| {
            let mut updated = (*tag_state).clone();
            updated.retain(|t| t.label != tag_label);
            on_change.emit(updated.clone());
            tag_state.set(updated);
        })
    };

    let on_input = {
        let new_tag = new_tag.clone();
        Callback::from(move |e: InputEvent| {
            let input: HtmlInputElement = e.target_unchecked_into();
            new_tag.set(input.value());
        })
    };

    let on_keypress = {
        // TODO
        // let new_tag = new_tag.clone();
        // let tag_state = tag_state.clone();
        // let on_change = on_change.clone();
        Callback::from(move |e: KeyboardEvent| {
            if e.key() == "Enter" {
                // let val = (*new_tag).trim().to_string();
                // if !val.is_empty() && !tag_state.contains(&val) {
                //     let mut updated = (*tag_state).clone();
                //     updated.push(val.clone());
                //     on_change.emit(updated.clone());
                //     tag_state.set(updated);
                // }
                // new_tag.set("".into());
            }
        })
    };

    html! {
    <div class="tp__tag_list">
        { for (*tag_state).iter().map(|tag| html! {
            <Chip label={tag.label.clone()} class={tag.class.clone()} removable={props.removable} on_remove={on_remove.clone()}/>
          })
        }
        {
            if allow_add {
                html! {
                    <input
                        class="tp__add-input"
                        type="text"
                        value={(*new_tag).clone()}
                        oninput={on_input.clone()}
                        onkeypress={on_keypress.clone()}
                        placeholder="Add tag..."
                    />
                }
            } else {
                html! {}
            }
        }
    </div>
}
}
