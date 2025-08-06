use std::ops::Deref;
use regex::Regex;
use shared::foundation::mapper::{MapperScript, Statement, Expression, ExprId, MapKey, BuiltInFunction, MapCase, AssignmentTarget, MatchCase, RegexSource, MapCaseKey};
use yew::prelude::*;

#[derive(Properties, PartialEq, Clone)]
pub struct MapperScriptViewProps {
    #[prop_or_default]
    pub pretty: bool,
    #[prop_or(false)]
    pub inline: bool,
    pub script: Option<MapperScript>,
}

#[function_component]
pub fn MapperScriptView(props: &MapperScriptViewProps) -> Html {
    html! {
        <div class={classes!("tp__mapper-script", if props.inline {"tp__mapper-script__inline"} else {""} )}>
            {
                match props.script.as_ref() {
                    Some(script) => html! {
                        <pre class="tp__mapper-script__code">
                            { render_script(script, props.pretty, 0, false, 1) }
                        </pre>
                    },
                    None => html! { },
                }
            }
        </div>
    }
}

struct FormatParams {
    pub pretty: bool,
    pub level: usize,
    pub do_indent: bool,
    pub p_count: usize,
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

fn newline(format_params: &FormatParams) -> Html {
    if format_params.pretty {
        html! { <br /> }
    } else {
        html!{}
    }
}

fn render_args(args: &[ExprId], script: &MapperScript, format_params: &FormatParams) -> Html {
    html! {
        <>
            {
                for args.iter().map(|expr_id| {
                    render_expression(expr_id, script, format_params)
                })
            }
        </>
    }
}

fn render_var_access(name: &str, field: &str) -> Html {
    html! { <span class="var-access">{ name }{ field }</span> }
}

fn render_field(field: &str) -> Html {
    html! { <span class="field">{"@"}{ field }</span> }
}

fn render_identifier(ident: &str) -> Html {
    html! { <span class="identifier">{ ident }</span> }
}

fn render_map_key(key: &MapKey) -> Html {
    match key {
        MapKey::Identifier(ident) => render_identifier(ident),
        MapKey::FieldAccess(field) => render_field(field),
        MapKey::VarAccess(name, field) => render_var_access(name, field),
    }
}

fn render_function_call(name: &BuiltInFunction, args: &[ExprId], script: &MapperScript, format_params: &FormatParams) -> Html {
    html! {
        <span class="built-in-function">{name.to_string()}{"("}{render_args(args, script, format_params)}{")"} </span>
    }
}

fn render_literal(literal: &str) -> Html {
    html! { <span class="literal">{"'"}{ literal }{"'"}</span> }
}

fn render_num_literal(literal: &f64) -> Html {
    html! { <span class="num-literal">{ literal }</span> }
}

fn render_null_value() -> Html {
    html! { <span class="null-value">{ "null" }</span> }
}

fn render_map_case(case: &MapCase, script: &MapperScript, format_params: &FormatParams) -> Html {
    let keys_html = html! {
        <>
            {
                for case.keys.iter().enumerate().map(|(i, key)| {
                    let item = match key {
                        MapCaseKey::Text(text) => render_literal(text),
                        MapCaseKey::RangeFrom(from) => html! { format!("{from}..") },
                        MapCaseKey::RangeTo(to) => html! { format!("..{to}") },
                        MapCaseKey::RangeFull(from, to) => html! { format!("{from}..{to}") },
                        MapCaseKey::RangeEq(val) => html! { val.to_string() },
                        MapCaseKey::AnyMatch => html! { "_" },
                    };

                    if i < case.keys.len() - 1 {
                        html! { <> { item } { ", " } </> }
                    } else {
                        html! { { item } }
                    }
                })
            }
        </>
    };
    let has_bracket = case.keys.len() > 1;
    html! {
        <>
            {if has_bracket {"("} else {""}}
            {keys_html}
            {if has_bracket {")"} else {""}}
            {" => "} {render_expression(&case.expression, script, format_params)}{","}
            {newline(format_params)}
        </>
    }
}


fn render_map_cases(cases: &[MapCase], script: &MapperScript, format_params: &FormatParams) -> Html {
    html!{
        <>
            {
                for cases.iter().map(|case| render_map_case(case, script, format_params))
            }
        </>
    }
}

fn render_map_block(map_key: &MapKey, cases: &[MapCase], script: &MapperScript, format_params: &FormatParams) -> Html {
    html! {
        <> {"map "}
            {render_map_key(map_key)}
            <span class="bracket">{" {"}</span>
            {newline(format_params)}
            {render_map_cases(cases, script, format_params)}
            {newline(format_params)}
            <span class="bracket">{"}"}</span>
            {newline(format_params)}
        </>
    }
}


fn render_block(expr_ids: &[ExprId], script: &MapperScript, format_params: &FormatParams) -> Html {
    html! {
        <>
          <span class="bracket">{"{"}
          </span>{ newline(format_params) }
            {
                for expr_ids.iter().map(|expr_id| {
                html! {
                    <>
                    {render_expression(expr_id, script, format_params)}
                    {newline(format_params)}
                    </>
                }})
            }
        { newline(format_params) }
         <span class="bracket">{"}"}</span>
        {newline(format_params)}

        </>
    }
}

fn render_assignment(target: &AssignmentTarget, expr_id: &ExprId, script: &MapperScript, format_params: &FormatParams) -> Html {
    let target_html = match target {
        AssignmentTarget::Identifier(ident) => render_identifier(ident),
        AssignmentTarget::Field(field) => render_field(field),
    };

    html! {
        <>
            { target_html } {" = "}
            { render_expression(expr_id, script, format_params) }
            { newline(format_params) }
        </>
    }
}

fn render_match_block(match_cases: &[MatchCase], script: &MapperScript, format_params: &FormatParams) -> Html {
    html! {
        <>
        {"!! TODO MATCH BLOCK !!!"}
        {newline(format_params)}
        </>
    }
}


fn render_regex_source(source: &RegexSource) -> Html {
    match source  {
        RegexSource::Identifier(ident) => render_identifier(ident),
        RegexSource::Field(field) => render_field(field),
    }
}

fn render_regexp(field: &RegexSource, pattern: &String, regex: &Regex) -> Html {
    html! { <> {render_regex_source(field)} {"~ '"}{ pattern } {"'"}  </> }
}

fn render_expression(expr_id: &ExprId, script: &MapperScript, format_params: &FormatParams) -> Html {
    script.get_expr_by_id(*expr_id.deref()).map(|expression| {
        match expression {
            Expression::Identifier(ident) => render_identifier(ident),
            Expression::StringLiteral(literal) => render_literal(literal),
            Expression::NumberLiteral(num) => render_num_literal(num),
            Expression::FieldAccess(field) => render_field(field),
            Expression::VarAccess(name, field) => render_var_access(name, field),
            Expression::RegexExpr { field, pattern, re_pattern } => render_regexp(field, pattern, re_pattern),
            Expression::FunctionCall { name, args } => render_function_call(name, args, script, format_params),
            Expression::Assignment { target, expr } => render_assignment(target, expr , script, format_params),
            Expression::MatchBlock(match_cases) => render_match_block(match_cases, script, format_params),
            Expression::MapBlock { key, cases} => render_map_block(key, cases, script, format_params),
            Expression::NullValue => render_null_value(),
            Expression::Block(expr_ids) => render_block(expr_ids, script, format_params),
        }
    }).unwrap_or_else(|| html! { <span class="expr-not-found">{"ExprNotFound"}</span> })
}

fn render_script(script: &MapperScript, pretty: bool, level: usize, do_indent: bool, p_count: usize) -> Html {
    let format_params = FormatParams {
        pretty, level, do_indent, p_count
    };
    let items = script.statements.iter().map(|stmt| {
        match stmt {
            Statement::Expression(expr_id) => html! {
                <>
                {render_expression(expr_id, script, &format_params)}
                {newline(&format_params)}
                </>
            },
            Statement::Comment(comment) => html!{ <pre>{ comment }</pre> },
        }
    });

    html! {
        <>
            { for items }
        </>
    }
}
