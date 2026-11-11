use wasm_bindgen::{closure::Closure, JsCast};
use web_sys::{Element, HtmlElement, ResizeObserver};
use yew::prelude::*;

const HEADER: &str = ".tp__playlist-update-view__input-card-header";
const FACTS: &str = ".tp__playlist-update-view__input-facts";
const ACTIONS: &str = ".tp__playlist-update-view__action-area";
const SUMMARY: &str = ".tp__playlist-update-view__update-details > summary";

fn child(card: &Element, selector: &str) -> Option<Element> { card.query_selector(selector).ok().flatten() }

fn align_cards(grid: &HtmlElement, cards: &[HtmlElement]) {
    let window = gloo_utils::window();
    let Some(grid_style) = window.get_computed_style(grid).ok().flatten() else { return };
    let columns = grid_style.get_property_value("grid-template-columns").unwrap_or_default();
    let column_count = columns.split_whitespace().count().max(1);

    for row in cards.chunks(column_count) {
        let ends: Vec<_> = row
            .iter()
            .map(|card| {
                child(card, FACTS).map_or(0.0, |facts| {
                    facts.get_bounding_client_rect().bottom() - card.get_bounding_client_rect().top()
                })
            })
            .collect();
        let last = ends.iter().copied().fold(0.0, f64::max);
        for (card, end) in row.iter().zip(ends) {
            let _ = card.style().set_property("--update-action-offset", &format!("{}px", last - end));
        }
    }

    // The disclosure summary marks the end of the closed content even when details are open.
    let height = cards
        .iter()
        .filter_map(|card| {
            let summary = child(card, SUMMARY)?;
            let style = window.get_computed_style(card).ok().flatten()?;
            let inset = |name| {
                style
                    .get_property_value(name)
                    .ok()
                    .and_then(|value| value.trim_end_matches("px").parse::<f64>().ok())
                    .unwrap_or(0.0)
            };
            Some(
                summary.get_bounding_client_rect().bottom() - card.get_bounding_client_rect().top()
                    + inset("padding-bottom")
                    + inset("border-bottom-width"),
            )
        })
        .fold(0.0, f64::max);
    let _ = grid.style().set_property("--closed-card-height", &format!("{}px", height.ceil()));
}

#[hook]
pub(super) fn use_update_card_layout(grid: NodeRef, card_ids: Vec<u16>) {
    use_effect_with(card_ids, move |_| {
        let subscription = grid.cast::<HtmlElement>().and_then(|grid| {
            let elements = grid.children();
            let cards: Vec<_> = (0..elements.length())
                .filter_map(|index| elements.item(index)?.dyn_into::<HtmlElement>().ok())
                .collect();
            align_cards(&grid, &cards);
            let callback = Closure::<dyn FnMut(js_sys::Array, ResizeObserver)>::new({
                let grid = grid.clone();
                let cards = cards.clone();
                move |_, _| align_cards(&grid, &cards)
            });
            let observer = ResizeObserver::new(callback.as_ref().unchecked_ref()).ok()?;
            observer.observe(&grid);
            for card in cards {
                for selector in [HEADER, FACTS, ACTIONS, SUMMARY] {
                    if let Some(part) = child(&card, selector) {
                        observer.observe(&part);
                    }
                }
            }
            Some((observer, callback))
        });
        move || {
            if let Some((observer, _callback)) = subscription {
                observer.disconnect();
            }
        }
    });
}
