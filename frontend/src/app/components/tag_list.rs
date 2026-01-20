use web_sys::HtmlInputElement;
use yew::prelude::*;
use std::rc::Rc;
use crate::app::components::chip::Chip;

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
    #[prop_or(true)]
    pub readonly: bool,
    #[prop_or_else(|| "Add tag...".to_string())]
    pub placeholder: String,
}

#[function_component]
pub fn TagList(props: &TagListProps) -> Html {
    let TagListProps {
        tags,
        on_change,
        readonly,
        placeholder,
    } = props.clone();

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

    // add new tag on enter
    let on_keydown = {
        let new_tag = new_tag.clone();
        let tag_state = tag_state.clone();
        let on_change = on_change.clone();
        Callback::from(move |e: KeyboardEvent| {
            if e.key() == "Enter" {
                e.prevent_default();
                e.stop_propagation();
                let val = (*new_tag).trim().to_string();
                if !val.is_empty() && !tag_state.iter().any(|t| t.label == val) {
                    let mut updated = (*tag_state).clone();
                    updated.push(Rc::new(Tag { label: val.clone(), class: None }));
                    on_change.emit(updated.clone());
                    tag_state.set(updated);
                }
                new_tag.set(String::new());
            }
        })
    };

    html! {
        <div class="tp__tag_list">
            { for (*tag_state).iter().map(|tag| html! {
                <Chip
                    label={tag.label.clone()}
                    class={tag.class.clone()}
                    removable={!readonly}
                    on_remove={if readonly { Callback::noop() } else { on_remove.clone() }}
                />
            })}
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
                    </div>
                    </div>
                    }
                }
            }
        </div>
    }
}
