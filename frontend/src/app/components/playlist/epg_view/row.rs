use super::{
    layout::min_to_px,
    model::{visible_programme_range, EpgGridChannel},
};
use crate::app::components::IconButton;
use std::rc::Rc;
use web_sys::MouseEvent;
use yew::{classes, component, html, AttrValue, Callback, Html, Properties};

/// Offset of the record button's left edge from the programme's right edge:
/// 4px box border, 2px gap, 18px button.
const RECORD_BUTTON_RIGHT_INSET: i64 = 24;
/// Smallest offset of the record button from the programme's left edge, so it
/// never starts before the programme it records.
const RECORD_BUTTON_LEFT_INSET: i64 = 4;

#[derive(Properties)]
pub(super) struct EpgRowProps {
    pub channel: Rc<EpgGridChannel>,
    pub from_min: i64,
    pub to_min: i64,
    pub pixels_per_min: f64,
    /// Current minute relative to the window start.
    pub now_min: i64,
    pub row_style: AttrValue,
    pub can_record: bool,
    /// `(channel key, programme index)`
    pub on_record: Callback<(usize, usize)>,
}

impl PartialEq for EpgRowProps {
    fn eq(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.channel, &other.channel)
            && self.from_min == other.from_min
            && self.to_min == other.to_min
            && self.pixels_per_min.to_bits() == other.pixels_per_min.to_bits()
            && self.now_min == other.now_min
            && self.row_style == other.row_style
            && self.can_record == other.can_record
            && self.on_record == other.on_record
    }
}

#[component]
pub(super) fn EpgRow(props: &EpgRowProps) -> Html {
    let channel = &props.channel;
    let ppm = props.pixels_per_min;
    let programmes = visible_programme_range(channel, props.from_min, props.to_min)
        .filter(|&idx| channel.programmes[idx].right_min > props.from_min)
        .map(|idx| {
            let programme = &channel.programmes[idx];
            let left = min_to_px(programme.left_min, ppm);
            let right = min_to_px(programme.right_min, ppm);
            let width = (right - left).max(0);
            let is_active = programme.left_min <= props.now_min && props.now_min < programme.right_min;
            let is_past = programme.right_min <= props.now_min;
            // The record button sits beside the programme box rather than inside it: the box
            // clips its content, which hid the button on every programme narrower than the button.
            let record_button = if props.can_record && !is_past {
                let on_record = props.on_record.clone();
                let key = channel.key;
                let button_left = (right - RECORD_BUTTON_RIGHT_INSET).max(left + RECORD_BUTTON_LEFT_INSET);
                html! {
                    <div class="tp__epg__program-record" style={format!("left:{button_left}px")}
                        onmousedown={Callback::from(|e: MouseEvent| e.stop_propagation())}>
                        <IconButton
                            name="program_record"
                            icon="DVR"
                            class="tp__epg__program-menu"
                            onclick={Callback::from(move |_: (String, MouseEvent)| on_record.emit((key, idx)))}
                        />
                    </div>
                }
            } else {
                html! {}
            };
            html! {
                <>
                <div class={classes!("tp__epg__program", is_active.then_some("tp__epg__program-active"))}
                     style={format!("left:{left}px;width:{width}px")} title={programme.title.to_string()}>
                    <div class="tp__epg__program-time">{ &*programme.time_label }</div>
                    <div class="tp__epg__program-title">{ &*programme.title }</div>
                </div>
                { record_button }
                </>
            }
        });
    html! {
        <div class="tp__epg__channel-programs" style={props.row_style.clone()}>{ for programmes }</div>
    }
}
