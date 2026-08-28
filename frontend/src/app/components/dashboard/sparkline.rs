use shared::utils::human_readable_byte_size;
use std::rc::Rc;
use yew::prelude::*;

const VIEW_W: f64 = 100.0;
const VIEW_H: f64 = 32.0;
const PAD_Y: f64 = 3.0;
const PAD_X: f64 = 2.5;

#[derive(Clone, PartialEq, Debug)]
pub enum SparklineFormat {
    Percent,
    BytesPerSec,
    Count,
}

impl SparklineFormat {
    fn render(&self, value: f64) -> String {
        match self {
            SparklineFormat::Percent => format!("{value:.1}%"),
            SparklineFormat::BytesPerSec => format!("{}/s", human_readable_byte_size(value.max(0.0) as u64)),
            SparklineFormat::Count => format!("{value:.0}"),
        }
    }
}

#[derive(Clone, PartialEq, Debug)]
pub struct SparklineSeries {
    pub values: Rc<[f64]>,
    pub class: String,
    pub label: String,
}

impl SparklineSeries {
    pub fn new(values: Rc<[f64]>) -> Self { Self { values, class: String::new(), label: String::new() } }

    pub fn with_class(mut self, class: impl Into<String>) -> Self {
        self.class = class.into();
        self
    }

    pub fn with_label(mut self, label: impl Into<String>) -> Self {
        self.label = label.into();
        self
    }
}

#[derive(Properties, Clone, PartialEq, Debug)]
pub struct SparklineProps {
    pub series: Rc<[SparklineSeries]>,
    #[prop_or_default]
    pub max: Option<f64>,
    #[prop_or(SparklineFormat::Count)]
    pub format: SparklineFormat,
    #[prop_or_default]
    pub class: String,
}

fn x_at(index: usize, count: usize) -> f64 {
    if count <= 1 {
        VIEW_W / 2.0
    } else {
        PAD_X + (index as f64) / ((count - 1) as f64) * (VIEW_W - 2.0 * PAD_X)
    }
}

fn point_at(values: &[f64], index: usize, lo: f64, hi: f64) -> Option<(f64, f64)> {
    let n = values.len();
    if index >= n {
        return None;
    }
    let usable_h = VIEW_H - 2.0 * PAD_Y;
    let x = x_at(index, n);
    let span = (hi - lo).max(f64::EPSILON);
    let ratio = ((values[index] - lo) / span).clamp(0.0, 1.0);
    let y = VIEW_H - PAD_Y - ratio * usable_h;
    Some((x, y))
}

fn build_points(values: &[f64], lo: f64, hi: f64) -> Vec<(f64, f64)> {
    (0..values.len()).filter_map(|i| point_at(values, i, lo, hi)).collect()
}

#[component]
pub fn Sparkline(props: &SparklineProps) -> Html {
    let hover = use_state(|| None::<(usize, f64)>);
    let container_ref = use_node_ref();
    let class = classes!("tp__sparkline", props.class.clone());

    let point_count = props.series.iter().map(|s| s.values.len()).max().unwrap_or(0);

    if point_count == 0 {
        return html! {
            <div ref={container_ref} class={class}>
                <svg class="tp__sparkline__svg" viewBox={format!("0 0 {VIEW_W} {VIEW_H}")} preserveAspectRatio="none" aria-hidden="true">
                    <line class="tp__sparkline__baseline" x1="0" y1={format!("{}", VIEW_H - PAD_Y)} x2={format!("{VIEW_W}")} y2={format!("{}", VIEW_H - PAD_Y)} vector-effect="non-scaling-stroke" />
                </svg>
            </div>
        };
    }

    let (lo, hi) = if let Some(m) = props.max {
        (0.0, m.max(f64::EPSILON))
    } else {
        let mut data_min = f64::INFINITY;
        let mut data_max = f64::NEG_INFINITY;
        for v in props.series.iter().flat_map(|s| s.values.iter().copied()) {
            data_min = data_min.min(v);
            data_max = data_max.max(v);
        }
        if !data_min.is_finite() || !data_max.is_finite() {
            (0.0, 1.0)
        } else {
            let span = (data_max - data_min).max(f64::EPSILON);
            let pad = span * 0.15;
            ((data_min - pad).max(0.0), data_max + pad)
        }
    };

    let onmousemove = {
        let hover = hover.clone();
        let container_ref = container_ref.clone();
        Callback::from(move |e: MouseEvent| {
            if point_count <= 1 {
                hover.set(Some((0, VIEW_W / 2.0)));
                return;
            }
            if let Some(target) = container_ref.cast::<web_sys::Element>() {
                let rect = target.get_bounding_client_rect();
                let width = rect.width();
                if width > 0.0 {
                    let x = f64::from(e.client_x()) - rect.left();
                    let ratio = (x / width).clamp(0.0, 1.0);
                    let idx = ((ratio * point_count as f64).floor() as usize).min(point_count - 1);
                    let pointer_x = (ratio * VIEW_W).clamp(0.0, VIEW_W);
                    hover.set(Some((idx, pointer_x)));
                }
            }
        })
    };
    let onmouseleave = {
        let hover = hover.clone();
        Callback::from(move |_| hover.set(None))
    };

    let series_svg = props
        .series
        .iter()
        .map(|s| {
            let points = build_points(s.values.as_ref(), lo, hi);
            if points.is_empty() {
                return Html::default();
            }
            let line_path = points
                .iter()
                .enumerate()
                .map(|(i, (x, y))| {
                    let cmd = if i == 0 { 'M' } else { 'L' };
                    format!("{cmd}{x:.2} {y:.2}")
                })
                .collect::<Vec<_>>()
                .join(" ");
            let (first_x, _) = points.first().copied().unwrap_or((0.0, VIEW_H));
            let (last_x, _) = points.last().copied().unwrap_or((VIEW_W, VIEW_H));
            let baseline = VIEW_H;
            let area_path = format!("{line_path} L{last_x:.2} {baseline:.2} L{first_x:.2} {baseline:.2} Z");
            let group_class = classes!("tp__sparkline__series", s.class.clone());
            html! {
                <g class={group_class}>
                    <path class="tp__sparkline__area" d={area_path} />
                    <path class="tp__sparkline__line" d={line_path} fill="none" vector-effect="non-scaling-stroke" />
                </g>
            }
        })
        .collect::<Html>();

    let dots_overlay = props
        .series
        .iter()
        .filter_map(|s| {
            let last = s.values.len().checked_sub(1)?;
            let (x, y) = point_at(s.values.as_ref(), last, lo, hi)?;
            let dot_class = classes!("tp__sparkline__dot", s.class.clone());
            let left_pct = x / VIEW_W * 100.0;
            let top_pct = y / VIEW_H * 100.0;
            Some(html! {
                <span class={dot_class} style={format!("left:{left_pct:.2}%;top:{top_pct:.2}%")} />
            })
        })
        .collect::<Html>();

    let hover_overlay = hover.and_then(|(idx, pointer_x)| {
        if point_count <= 1 {
            return None;
        }
        let cursor_x = pointer_x;
        let left_pct = cursor_x / VIEW_W * 100.0;
        let tip_align = if left_pct > 66.0 {
            "tp__sparkline__tooltip--end"
        } else if left_pct < 34.0 {
            "tp__sparkline__tooltip--start"
        } else {
            ""
        };

        let top_y = props
            .series
            .iter()
            .filter_map(|s| point_at(s.values.as_ref(), idx, lo, hi).map(|(_, y)| y))
            .fold(VIEW_H, f64::min);
        let top_pct = (top_y / VIEW_H * 100.0).clamp(0.0, 100.0);
        let markers = props
            .series
            .iter()
            .filter_map(|s| {
                let (x, y) = point_at(s.values.as_ref(), idx, lo, hi)?;
                let marker_class = classes!("tp__sparkline__marker", s.class.clone());
                let m_left = x / VIEW_W * 100.0;
                let m_top = y / VIEW_H * 100.0;
                Some(html! {
                    <span class={marker_class} style={format!("left:{m_left:.2}%;top:{m_top:.2}%")} />
                })
            })
            .collect::<Html>();

        let tooltip_rows = props
            .series
            .iter()
            .filter_map(|s| {
                let value = s.values.get(idx).copied()?;
                let swatch_class = classes!("tp__sparkline__swatch", s.class.clone());
                Some(html! {
                    <div class="tp__sparkline__tip-row">
                        <span class={swatch_class}></span>
                        { if s.label.is_empty() { Html::default() } else { html! { <span class="tp__sparkline__tip-label">{ s.label.clone() }</span> } } }
                        <span class="tp__sparkline__tip-value">{ props.format.render(value) }</span>
                    </div>
                })
            })
            .collect::<Html>();

        Some(html! {
            <>
                <svg class="tp__sparkline__svg tp__sparkline__cursor-svg" viewBox={format!("0 0 {VIEW_W} {VIEW_H}")} preserveAspectRatio="none" aria-hidden="true">
                    <line class="tp__sparkline__cursor" x1={format!("{cursor_x:.2}")} y1={format!("{top_y:.2}")} x2={format!("{cursor_x:.2}")} y2={format!("{VIEW_H}")} vector-effect="non-scaling-stroke" />
                </svg>
                { markers }
                <div class={classes!("tp__sparkline__tooltip", tip_align)} style={format!("left:{left_pct:.2}%;top:{top_pct:.2}%")}>
                    { tooltip_rows }
                </div>
            </>
        })
    });

    html! {
        <div ref={container_ref} class={class} onmousemove={onmousemove} onmouseleave={onmouseleave}>
            <svg class="tp__sparkline__svg" viewBox={format!("0 0 {VIEW_W} {VIEW_H}")} preserveAspectRatio="none" aria-hidden="true">
                { series_svg }
            </svg>
            { dots_overlay }
            { hover_overlay.unwrap_or_default() }
        </div>
    }
}
