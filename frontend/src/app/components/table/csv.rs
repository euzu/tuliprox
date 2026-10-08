use super::{visible_columns, TableColumn, TableShell};
use crate::{app::components::NoContent, i18n::use_translation, provider::use_user_settings};
use std::{collections::BTreeMap, rc::Rc};
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
    pub table_namespace: AttrValue,
    pub schema_version: AttrValue,
}

fn csv_schema(rows: &[Vec<String>], header: bool, namespace: &str, version: &str) -> (String, Rc<Vec<TableColumn>>) {
    let width = rows.iter().map(Vec::len).max().unwrap_or(0);
    let headers = rows.first().filter(|_| header);
    let mut frequencies = BTreeMap::<&str, usize>::new();
    if let Some(headers) = headers {
        for name in headers {
            *frequencies.entry(name).or_default() += 1;
        }
    }
    let extra = width.saturating_sub(headers.map_or(0, Vec::len));
    let schema = serde_json::to_vec(&(namespace, version, header, &frequencies, extra)).unwrap_or_default();
    let table = format!("{namespace}.{}", blake3::hash(&schema).to_hex());
    let mut occurrences = BTreeMap::<&str, usize>::new();
    let columns = (0..width)
        .map(|index| {
            if let Some(name) = headers.and_then(|headers| headers.get(index)) {
                let occurrence = occurrences.entry(name).or_default();
                *occurrence += 1;
                TableColumn::new(
                    format!("header.{}.{occurrence}", blake3::hash(name.as_bytes()).to_hex()),
                    name.clone(),
                )
            } else {
                TableColumn::new(format!("position.{index}"), format!("{}", index + 1))
            }
        })
        .collect();
    (table, Rc::new(columns))
}

#[component]
pub fn CsvTable(props: &CsvTableProps) -> Html {
    let rows =
        use_memo((props.content.clone(), props.separator), |(content, separator)| parse_csv(content, *separator));
    let schema = use_memo(
        (rows.clone(), props.first_row_is_header, props.table_namespace.clone(), props.schema_version.clone()),
        |(rows, header, namespace, version)| csv_schema(rows, *header, namespace, version),
    );
    let (table_id, columns) = &*schema;
    let context = use_user_settings();
    let translate = use_translation();
    let supported = columns.len() <= shared::model::SETTINGS_MAX_COLUMNS;
    let default_layout = shared::model::TableLayoutPreferencesDto::default();
    let layout = context
        .as_ref()
        .filter(|_| supported)
        .and_then(|context| context.state.settings.preferences.web_ui.tables.get(table_id))
        .unwrap_or(&default_layout);
    let visible = visible_columns(columns, layout);
    let data = if props.first_row_is_header { rows.get(1..).unwrap_or_default() } else { rows.as_slice() };
    let table_class = props.class.clone().unwrap_or_else(|| "tp__csv-table__table".into());
    html! { <div class="tp__csv-table tp__table">
        <TableShell table_id={table_id.clone()} columns={columns.clone()} supported={supported}>
            <table class={classes!("tp__table__table", table_class)}>
                <thead><tr>{for visible.iter().map(|index| { let column = &columns[*index]; html! { <th key={column.id.as_str()} data-column-id={column.id.clone()}>{column.label.resolve(|key| translate.t(key))}</th> } })}</tr></thead>
                <tbody>
                    if data.is_empty() { <tr><td colspan={visible.len().max(1).to_string()}><NoContent/></td></tr> }
                    {for data.iter().enumerate().map(|(row_index, row)| html! { <tr key={row_index}>
                        {for visible.iter().map(|index| { let column = &columns[*index]; html! { <td key={column.id.as_str()} data-column-id={column.id.clone()}>{row.get(*index).map(|value| value.trim().to_owned()).unwrap_or_default()}</td> } })}
                    </tr> })}
                </tbody>
            </table>
        </TableShell>
    </div> }
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
                current.push(field.clone());
                field.clear();
            }
            '\r' => {
                // ignore CR; line end  is '\n'
            }
            '\n' if !in_quotes => {
                current.push(field.clone());
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
        current.push(field.clone());
        rows.push(current);
    }

    // Trailing empty lines are ignored
    rows.into_iter().filter(|r| r.iter().any(|c| !c.is_empty())).collect()
}

#[cfg(test)]
#[path = "csv.test.rs"]
mod schema_tests;
