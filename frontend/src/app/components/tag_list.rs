use crate::app::components::{chip::Chip, IconButton};
use std::rc::Rc;
use web_sys::HtmlInputElement;
use yew::prelude::*;

#[derive(Clone, PartialEq, Debug)]
pub struct Tag {
    pub label: String,
    pub class: Option<String>,
}

fn default_create_tag(value: String) -> Option<Tag> { Some(Tag { label: value, class: None }) }

#[derive(Properties, Clone, PartialEq)]
pub struct TagListProps {
    pub tags: Vec<Rc<Tag>>,
    #[prop_or_else(Callback::noop)]
    pub on_change: Callback<Vec<Rc<Tag>>>,
    #[prop_or_else(|| Callback::from(default_create_tag))]
    pub create_tag: Callback<String, Option<Tag>>,
    #[prop_or(true)]
    pub readonly: bool,
    #[prop_or_else(|| "Add tag...".to_string())]
    pub placeholder: String,
}

#[component]
pub fn TagList(props: &TagListProps) -> Html {
    let TagListProps { tags, on_change, create_tag, readonly, placeholder } = props.clone();

    let tag_state = use_state(|| tags.clone());
    let new_tag = use_state(String::default);

    // keep local state in sync when parent updates
    {
        let tag_state = tag_state.clone();
        use_effect_with(tags.clone(), move |tags| {
            tag_state.set(tags.clone());
            || ()
        });
    }

    // remove existing tag
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

    // input change for new tag
    let on_input = {
        let new_tag = new_tag.clone();
        Callback::from(move |e: InputEvent| {
            let input: HtmlInputElement = e.target_unchecked_into();
            new_tag.set(input.value());
        })
    };

    let add_tag = {
        let new_tag = new_tag.clone();
        let tag_state = tag_state.clone();
        let on_change = on_change.clone();
        let create_tag = create_tag.clone();
        Callback::from(move |()| {
            let val = (*new_tag).trim().to_string();
            if val.is_empty() {
                return;
            }

            if let Some(next_tag) = create_tag.emit(val.clone()) {
                if !tag_state.iter().any(|t| t.label == next_tag.label) {
                    let mut updated = (*tag_state).clone();
                    updated.push(Rc::new(next_tag));
                    on_change.emit(updated.clone());
                    tag_state.set(updated);
                    new_tag.set(String::new());
                }
            }
        })
    };

    // add new tag on enter
    let on_keydown = {
        let add_tag = add_tag.clone();
        Callback::from(move |e: KeyboardEvent| {
            if e.key() == "Enter" && !e.is_composing() {
                e.prevent_default();
                e.stop_propagation();
                add_tag.emit(());
            }
        })
    };

    let on_add_tag = {
        let add_tag = add_tag.clone();
        Callback::from(move |(_, e): (String, MouseEvent)| {
            e.prevent_default();
            e.stop_propagation();
            add_tag.emit(());
        })
    };

    html! {
        <div class="tp__tag_list">
            for tag in (*tag_state).iter() {
                <Chip
                    key={tag.label.clone()}
                    label={tag.label.clone()}
                    class={tag.class.clone()}
                    removable={!readonly}
                    on_remove={if readonly { Callback::noop() } else { on_remove.clone() }}
                />
            }
            {
                if readonly {
                    html! {}
                } else {
                    html! {
                    <div class="tp__input">
                    <div class="tp__input-wrapper">
                        <input
                            type="text"
                            value={(*new_tag).clone()}
                            oninput={on_input.clone()}
                            onkeydown={on_keydown.clone()}
                            placeholder={placeholder}
                        />
                        <IconButton name="add" icon="Add" onclick={on_add_tag.clone()}/>
                    </div>
                    </div>
                    }
                }
            }
        </div>
    }
}
