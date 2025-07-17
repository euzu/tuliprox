use shared::foundation::filter::Filter;
use yew::prelude::*;

#[derive(Properties, PartialEq, Clone)]
pub struct FilterViewProps {
    #[prop_or_default]
    pub pretty: bool,
    #[prop_or(false)]
    pub inline: bool,
    pub filter: Option<Filter>,
}

#[function_component]
pub fn FilterView(props: &FilterViewProps) -> Html {
    html! {
        <div class={classes!("tp__filter", if props.inline {"tp__filter__inline"} else {""} )}>
            {
                match props.filter.as_ref() {
                    Some(filter) => html! {
                        <pre class="tp__filter__code">
                            { render_filter(filter, props.pretty, 0, false, 1) }
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
        html!{}
    }
}

fn render_filter(filter: &Filter, pretty: bool, level: usize, do_indent: bool, p_count: usize) -> Html {
    match filter {
        Filter::Group(inner) => {
            html! {
            <>
                { indent(level, do_indent &&  pretty) }
                <span class={format!("bracket bracket-{}", p_count % 6)}>{"("}</span>
                {newline(pretty)}
                { indent(level +1 , pretty) }
                { render_filter(inner, pretty, level + 1, false, p_count+1) }
                {newline(pretty)}
                { indent(level , pretty) }
                <span class={format!("bracket bracket-{}", p_count % 6)}>{ ")" }</span>
            </>
         }
        }
        Filter::FieldComparison(field, regex) => html! {
            <>
               { indent(level, do_indent &&  pretty) }
                <span class="comparison">
                    <span class="field">{format!("{:?}", field)}</span>
                    {" ~ "}
                    <span class="regex">{format!("\"{}\"", regex.restr)}</span>
                </span>
            </>
        },
        Filter::TypeComparison(field, t) => html! {
            <>
               { indent(level, do_indent && pretty) }
                <span class="comparison">
                    <span class="field">{format!("{:?}", field)}</span>{" = "}
                    <span class="enum">{format!("{:?}", t)}</span>
                </span>
            </>
        },
        Filter::UnaryExpression(op, inner) => {
            html! {
                <>
                    { indent(level, do_indent && pretty) }
                    <span class="unary_op">{format!(" {:?} ", op)}</span>
                    {newline(pretty)}
                    { indent(level, pretty) }
                    { render_filter(inner, pretty, level, do_indent && pretty, p_count) }
                </>
            }
        },
        Filter::BinaryExpression(left, op, right) => html! {
            <span class="binary_op-wrapper">
                 { render_filter(left, pretty, level, do_indent && pretty, p_count) }
                 { newline(pretty) }
                 { indent(level, pretty) }
                 <span class="binary_op">{format!(" {:?} ", op)}</span>
                 { newline(pretty) }
                 { render_filter(right, pretty, level, pretty, p_count) }
            </span>
        },
    }
}
