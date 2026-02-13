use crate::app::components::NoContent;
use yew::prelude::*;

#[derive(Properties, PartialEq, Clone)]
pub struct CsvTableProps {
    pub content: String,
    #[prop_or(';')]
    pub separator: char,
    #[prop_or(true)]
    pub first_row_is_header: bool,
    #[prop_or_default]
    pub class: Option<String>,
}

#[function_component]
pub fn CsvTable(props: &CsvTableProps) -> Html {
    let separator = props.separator;

    let rows = use_memo((props.content.clone(), separator), |(content, sep)| {
        parse_csv(content, *sep)
    });

    if rows.is_empty() {
        return html! { <NoContent/> };
    }

    let (header, data) = if props.first_row_is_header && !rows.is_empty() {
        (Some(rows[0].clone()), rows[1..].to_vec())
    } else {
        (None, rows.to_vec())
    };

    let table_class = props
        .class
        .clone()
        .unwrap_or_else(|| "tp__csv-table__table".to_string());

    html! {
        <div class="tp__csv-table tp__table">
        <div class="tp__table__container">
            <table class={classes!("tp__table__table", table_class)}>
                {
                    if let Some(h) = header {
                        html! {
                            <thead>
                                <tr>
                                    {
                                        h.into_iter().map(|cell| html!{
                                            <th>{ cell }</th>
                                        }).collect::<Html>()
                                    }
                                </tr>
                            </thead>
                        }
                    } else {
                        html! {}
                    }
                }
                <tbody>
                    {
                        data.into_iter().map(|row| html!{
                            <tr>
                                {
                                    row.into_iter().map(|cell| html!{
                                        <td>{ cell }</td>
                                    }).collect::<Html>()
                                }
                            </tr>
                        }).collect::<Html>()
                    }
                </tbody>
            </table>
        </div>
        </div>
    }
}

fn parse_csv(input: &str, separator: char) -> Vec<Vec<String>> {
    let mut rows = Vec::new();
    let mut current = Vec::new();
    let mut field = String::new();
    let mut in_quotes = false;
    let mut chars = input.trim_start_matches('\u{FEFF}').chars().peekable(); // remove BOM

    while let Some(c) = chars.next() {
        match c {
            '"' => {
                if in_quotes {
                    // Double quote => escaped quote
                    if let Some('"') = chars.peek().copied() {
                        chars.next();
                        field.push('"');
                    } else {
                        in_quotes = false;
                    }
                } else {
                    in_quotes = true;
                }
            }
            ch if ch == separator && !in_quotes => {
                current.push(field.trim().to_string());
                field.clear();
            }
            '\r' => {
                // ignore CR; line end  is '\n'
            }
            '\n' if !in_quotes => {
                current.push(field.trim().to_string());
                field.clear();
                // skip last empty line
                if !(current.is_empty() || current.len() == 1 && current[0].is_empty()) {
                    rows.push(current);
                }
                current = Vec::new();
            }
            other => field.push(other),
        }
    }

    // append last field
    if in_quotes {
        // Unbalanced Quotes: we take the remaining field as is
    }
    if !field.is_empty() || !current.is_empty() {
        current.push(field.trim().to_string());
        rows.push(current);
    }

    // Trailing empty lines are ignored
    rows.into_iter()
        .filter(|r| r.iter().any(|c| !c.is_empty()))
        .collect()
}
