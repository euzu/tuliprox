use std::rc::Rc;
use std::str::FromStr;
use wasm_bindgen::JsCast;
use yew::prelude::*;
use crate::app::components::AppIcon;

#[derive(Properties, PartialEq, Clone)]
pub struct BreadcrumbsProps {
    pub items: Rc<Vec<String>>,
    #[prop_or_default]
    pub onclick: Callback<(String, usize)>,
}
#[function_component]
pub fn Breadcrumbs(props: &BreadcrumbsProps) -> Html {
    let len = props.items.len();

    let handle_click = {
        let click = props.onclick.clone();
        Callback::from(move |e: MouseEvent| {
            e.prevent_default();
            if let Some(target) = e.target() {
                if let Ok(element) = target.dyn_into::<web_sys::Element>() {
                    if let Some(data_name) = element.get_attribute("data-name") {
                        if let Some(data_index) = element.get_attribute("data-index") {
                            if let Ok(index) = usize::from_str(&data_index) {
                                click.emit((data_name, index));
                            }
                        }
                    }
                }
            }
        })
    };

    html! {
        <nav class="tp__breadcrumbs" aria-label="Breadcrumb">
            <ol>
                { for props.items.iter().enumerate().map(|(i, item)| {
                    html! {
                        <li>
                        {
                            if i > 0 {
                              html! { <span class="tp__breadcrumbs__icon"><AppIcon name="ChevronRight"/></span> }
                            } else {
                                html! {}
                            }
                        }
                        {
                            if i < len -1 {
                               html! { <span class="tp__breadcrumbs__selectable"
                                data-index={i.to_string()}
                                data-name={ item.to_string() }
                                onclick={ handle_click.clone() }>{ &item }</span>
                              }
                            } else {
                               html! { <span class="tp__breadcrumbs__active">{ &item }</span> }
                            }
                        }
                        </li>
                    }
                })}
            </ol>
        </nav>
    }
}
