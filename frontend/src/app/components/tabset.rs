use crate::app::components::{IconButton, Panel, TextButton};
use shared::utils::Internable;
use std::rc::Rc;
use yew::prelude::*;

#[derive(Clone, Debug, PartialEq)]
pub struct TabItem {
    pub id: String,
    pub title: String,
    pub icon: String,
    pub children: Html,
    pub active_class: Option<String>,
    pub inactive_class: Option<String>,
}

#[derive(Properties, Clone, PartialEq)]
pub struct TabSetProps {
    pub tabs: Rc<Vec<TabItem>>,
    #[prop_or_default]
    pub class: String,
    #[prop_or_default]
    pub active_tab: Option<String>,
    #[prop_or_default]
    pub on_tab_change: Option<Callback<String>>,
}

#[component]
pub fn TabSet(props: &TabSetProps) -> Html {
    let active_tab = use_state(|| {
        props.active_tab.clone().or_else(|| props.tabs.first().map(|tab| tab.id.clone())).unwrap_or_default()
    });

    // Update active tab when prop changes
    {
        let active_tab_state = active_tab.clone();
        let prop_active = props.active_tab.clone();
        use_effect_with(prop_active, move |new_active| {
            if let Some(new_tab) = new_active {
                if &*active_tab_state != new_tab {
                    active_tab_state.set(new_tab.clone());
                }
            }
        });
    }

    let handle_tab_click = {
        let active_tab_state = active_tab.clone();
        let on_change = props.on_tab_change.clone();
        Callback::from(move |tab_id: String| {
            active_tab_state.set(tab_id.clone());
            if let Some(callback) = &on_change {
                callback.emit(tab_id);
            }
        })
    };

    // Arrow/Home/End keyboard navigation over the tablist
    let handle_header_keydown = {
        let tabs = props.tabs.clone();
        let active_tab = active_tab.clone();
        let handle_tab_click = handle_tab_click.clone();
        Callback::from(move |event: KeyboardEvent| {
            let count = tabs.len();
            if count == 0 {
                return;
            }
            let current = tabs.iter().position(|t| t.id == *active_tab).unwrap_or(0);
            let next = match event.key().as_str() {
                "ArrowRight" | "ArrowDown" => Some((current + 1) % count),
                "ArrowLeft" | "ArrowUp" => Some((current + count - 1) % count),
                "Home" => Some(0),
                "End" => Some(count - 1),
                _ => None,
            };
            if let Some(idx) = next {
                event.prevent_default();
                handle_tab_click.emit(tabs[idx].id.clone());
            }
        })
    };

    let render_tab_buttons = {
        let tabs = props.tabs.clone();
        let active_tab_id = (*active_tab).clone();
        let handle_click = handle_tab_click.clone();
        let render_tab_button = |tab: &TabItem| {
            let tab_id = tab.id.clone();
            let is_active = tab_id == active_tab_id;
            let click_handler = handle_click.clone();

            html! {
                <div key={tab.id.clone()} role="presentation" class={classes!(
                    "tp__tab-set__tab",
                    if is_active { tab.active_class.as_ref().map_or("tp__tab-set__tab--active".to_string(), |s| s.clone())
                    } else {  tab.inactive_class.as_ref().map_or_else(String::new, |s| s.clone())  }
                )}>
                    // Desktop: TextButton
                    <TextButton
                        name={tab_id.clone()}
                        title={tab.title.clone()}
                        icon={tab.icon.clone()}
                        class={if is_active { "tp__tab-set__tab-desktop active" } else { "tp__tab-set__tab-desktop" }}
                        onclick={
                            let click_handler = click_handler.clone();
                            Callback::from(move |name: String| {
                                click_handler.emit(name);
                            })
                        }
                    />

                    // Mobile: IconButton
                    <div class="tp__tab-set__tab-mobile">
                        <IconButton
                            name={tab_id}
                            icon={tab.icon.clone()}
                            class={if is_active { "active" } else { "" }}
                            onclick={
                                let click_handler = click_handler.clone();
                                Callback::from(move |(name, _): (String, MouseEvent)| {
                                    click_handler.emit(name);
                                })
                            }
                        />
                    </div>
                </div>
            }
        };

        html! {
            <div class="tp__tab-set__header" role="tablist" tabindex="0" onkeydown={handle_header_keydown.clone()}>
                for tab in tabs.iter() {
                    { render_tab_button(tab) }
                }
            </div>
        }
    };

    let render_tab_content = {
        let tabs = props.tabs.clone();
        let active_tab_id = (*active_tab).clone().intern();

        html! {
            <div class="tp__tab-set__body">
                for tab in tabs.iter() {
                    <Panel
                        key={tab.id.clone()}
                        class="tp__tab-set__panel"
                        value={tab.id.clone().intern()}
                        active={active_tab_id.clone()}
                    >
                        { tab.children.clone() }
                    </Panel>
                }
            </div>
        }
    };

    html! {
        <div class={classes!("tp__tab-set", props.class.clone())}>
            { render_tab_buttons }
            { render_tab_content }
        </div>
    }
}
