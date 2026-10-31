mod bar_row;
mod epg_coverage_panel;
mod epg_coverage_ring;
mod epg_source_list;
mod input_output_panel;
mod kpi_card;
mod kpi_row;
mod missing_epg_chips;
mod playlist_stats_view;
mod stats_meta_line;
mod targets_table;

pub use playlist_stats_view::*;

/// Renders the `(before, value, after)` parts of [`crate::model::t_fmt_split`] with the value emphasized.
fn emphasize((before, value, after): (String, String, String)) -> yew::Html {
    yew::html! { <>{before}<strong>{value}</strong>{after}</> }
}
