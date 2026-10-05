use shared::model::EpgGroupInfo;
use std::{rc::Rc, sync::Arc};
use wasm_bindgen::JsCast;
use web_sys::{HtmlElement, KeyboardEvent};
use yew::{classes, component, html, AttrValue, Callback, Html, Properties};

#[derive(Properties, PartialEq)]
pub(super) struct EpgGroupListProps {
    pub title: AttrValue,
    pub groups: Rc<Vec<EpgGroupInfo>>,
    pub selected: Option<Arc<str>>,
    pub disabled: bool,
    pub on_select: Callback<Arc<str>>,
}

/// Moves keyboard focus to the previous or next list item.
fn focus_sibling(event: &KeyboardEvent, forward: bool) {
    let Some(current) = event.target().and_then(|target| target.dyn_into::<HtmlElement>().ok()) else { return };
    let sibling = if forward { current.next_element_sibling() } else { current.previous_element_sibling() };
    if let Some(sibling) = sibling.and_then(|element| element.dyn_into::<HtmlElement>().ok()) {
        let _ = sibling.focus();
    }
}

#[component]
pub(super) fn EpgGroupList(props: &EpgGroupListProps) -> Html {
    let items = props.groups.iter().map(|group| {
        let is_active = props.selected.as_deref() == Some(&*group.name);
        let select = {
            let on_select = props.on_select.clone();
            let name = Arc::clone(&group.name);
            let disabled = props.disabled;
            move || {
                if !disabled {
                    on_select.emit(Arc::clone(&name));
                }
            }
        };
        let onclick = {
            let select = select.clone();
            Callback::from(move |_| select())
        };
        let onkeydown = Callback::from(move |event: KeyboardEvent| match event.key().as_str() {
            "ArrowDown" => {
                event.prevent_default();
                focus_sibling(&event, true);
            }
            "ArrowUp" => {
                event.prevent_default();
                focus_sibling(&event, false);
            }
            "Enter" | " " => {
                event.prevent_default();
                select();
            }
            _ => {}
        });
        let label = format!("{} ({})", group.name, group.channel_count);
        html! {
            <li
                class={classes!("tp__epg__group", is_active.then_some("tp__epg__group--active"))}
                role="option"
                aria-selected={is_active.to_string()}
                tabindex="0"
                title={label.clone()}
                {onclick}
                {onkeydown}
            >
                <span class="tp__epg__group-name">{ &*group.name }</span>
                <span class="tp__epg__group-count">{ group.channel_count }</span>
            </li>
        }
    });
    html! {
        <div class={classes!("tp__epg__groups", props.disabled.then_some("tp__epg__groups--disabled"))}>
            <div class="tp__epg__groups-title" title={props.title.clone()}>{ props.title.clone() }</div>
            <ul class="tp__epg__group-list" role="listbox" aria-label={props.title.clone()}>
                { for items }
            </ul>
        </div>
    }
}
