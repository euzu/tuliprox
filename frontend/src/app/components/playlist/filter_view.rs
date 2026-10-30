use shared::foundation::{Filter, Leaf, Node, NodeId};
use yew::prelude::*;

#[derive(Properties, PartialEq, Clone)]
pub struct FilterViewProps {
    #[prop_or_default]
    pub pretty: bool,
    #[prop_or(false)]
    pub inline: bool,
    pub filter: Option<Filter>,
}

#[component]
pub fn FilterView(props: &FilterViewProps) -> Html {
    html! {
        <div class={classes!("tp__filter", if props.inline {"tp__filter__inline"} else {""} )}>
            {
                match props.filter.as_ref() {
                    Some(filter) => html! {
                        <pre class="tp__filter__code">
                            { Render { filter, pretty: props.pretty }.node(filter.root(), 0, false, 1) }
                        </pre>
                    },
                    None => html! { },
                }
            }
        </div>
    }
}

// Indents with spaces for pretty printing
fn indent(level: usize, do_indent: bool) -> Html {
    if do_indent {
        let spaces: AttrValue = " ".repeat(level * 2).into();
        html! { <>{ spaces }</> }
    } else {
        html! {}
    }
}

fn newline(pretty: bool) -> Html {
    if pretty {
        html! { <br /> }
    } else {
        html! {}
    }
}

/// Renders a filter tree. `pretty` is constant for one rendered filter, so it lives on the
/// struct instead of being passed through every call.
struct Render<'a> {
    filter: &'a Filter,
    pretty: bool,
}

impl Render<'_> {
    fn node(&self, node_id: NodeId, level: usize, do_indent: bool, p_count: usize) -> Html {
        let pretty = self.pretty;
        let filter = self.filter;
        let Some(node) = filter.node(node_id) else { return html! {} };
        match node {
            Node::Group(inner) => {
                html! {
                   <>
                       { indent(level, do_indent &&  pretty) }
                       <span class={format!("bracket bracket-{}", p_count % 6)}>{"("}</span>
                       { self.node(*inner, level, false, p_count + 1) }
                       <span class={format!("bracket bracket-{}", p_count % 6)}>{ ")" }</span>
                   </>
                }
            }
            Node::Leaf(leaf_id) => match filter.leaf(*leaf_id) {
                Some(Leaf::FieldComparison(field, regex)) => html! {
                    <>
                       { indent(level, do_indent &&  pretty) }
                        <span class="comparison">
                            <span class="field">{format!("{}", field)}</span>
                            {" ~ "}
                            <span class="regex">{format!("\"{}\"", regex.restr)}</span>
                        </span>
                    </>
                },
                Some(Leaf::TypeComparison(field, t)) => html! {
                    <>
                       { indent(level, do_indent && pretty) }
                        <span class="comparison">
                            <span class="field">{format!("{:?}", field)}</span>{" = "}
                            <span class="enum">{
                                {
                                   match t {
                                        shared::model::PlaylistItemType::Live => "live",
                                        shared::model::PlaylistItemType::Video
                                        | shared::model::PlaylistItemType::LocalVideo => "movie",
                                        shared::model::PlaylistItemType::Series
                                        | shared::model::PlaylistItemType::SeriesInfo
                                        | shared::model::PlaylistItemType::LocalSeries
                                        | shared::model::PlaylistItemType::LocalSeriesInfo => "series",
                                        _ => "unsupported"
                                    }
                                }
                            }</span>
                        </span>
                    </>
                },
                Some(Leaf::StringComparison(field, op, value)) => html! {
                    <>
                       { indent(level, do_indent && pretty) }
                        <span class="comparison">
                            <span class="field">{format!("{}", field)}</span>
                            {format!(" {} ", op)}
                            <span class="regex">{format!("\"{}\"", value)}</span>
                        </span>
                    </>
                },
                Some(Leaf::PresenceComparison(field, op)) => html! {
                    <>
                       { indent(level, do_indent && pretty) }
                        <span class="comparison">
                            <span class="field">{format!("{}", field)}</span>
                            {format!(" {}", op)}
                        </span>
                    </>
                },
                Some(Leaf::NumericComparison(field, op, value)) => html! {
                    <>
                       { indent(level, do_indent && pretty) }
                        <span class="comparison">
                            <span class="field">{format!("{}", field)}</span>
                            {format!(" {} ", op)}
                            <span class="enum">{value.to_string()}</span>
                        </span>
                    </>
                },
                Some(Leaf::SetComparison(field, values)) => html! {
                    <>
                       { indent(level, do_indent && pretty) }
                        <span class="comparison">
                            <span class="field">{format!("{}", field)}</span>
                            {" IN ["}
                            <span class="regex">{values.iter().map(|v| format!("\"{v}\"")).collect::<Vec<_>>().join(", ")}</span>
                            {"]"}
                        </span>
                    </>
                },
                None => html! {},
            },
            Node::Not(inner) => html! {
                <>
                    { indent(level, do_indent && pretty) }
                    <span class="unary_op">{" NOT "}</span>
                    { self.node(*inner, level, false, p_count) }
                </>
            },
            Node::And(..) | Node::Or(..) => self.chain(node_id, level, do_indent, p_count),
        }
    }

    /// Renders an `AND`/`OR` chain with a loop. The parser nests a chain on its right operand,
    /// so recursing on it would need stack proportional to the chain length.
    fn chain(&self, mut node_id: NodeId, level: usize, do_indent: bool, p_count: usize) -> Html {
        let pretty = self.pretty;
        let mut parts = Vec::new();
        let mut first = true;
        loop {
            let (left, op, right) = match self.filter.node(node_id) {
                Some(Node::And(left, right)) => (*left, "AND", *right),
                Some(Node::Or(left, right)) => (*left, "OR", *right),
                _ => break,
            };
            parts.push(self.node(left, level, first && do_indent && pretty, p_count));
            parts.push(newline(pretty));
            parts.push(indent(level, pretty));
            parts.push(html! { <span class="binary_op">{format!(" {op} ")}</span> });
            first = false;
            node_id = right;
        }
        parts.push(self.node(node_id, level, false, p_count));
        html! { <span class="binary_op-wrapper">{ for parts }</span> }
    }
}
