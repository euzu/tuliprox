use crate::{
    app::components::{
        convert_bool_to_chip_style, make_translated_header_callback, AppIcon, BatchInputContentView, Chip,
        EpgConfigView, HideContent, InputHeaders, InputOptions, InputTypeView, RevealContent, Table, TableDefinition,
    },
    html_if,
    i18n::use_translation,
};
use shared::{
    model::{ConfigInputAliasDto, ConfigInputDto, SortOrder},
    utils::unix_ts_to_str,
};
use std::rc::Rc;
use yew::prelude::*;

const HEADERS: [&str; 15] = [
    "LABEL.ENABLED",
    "LABEL.NAME",
    "LABEL.INPUT_TYPE",
    "LABEL.URL",
    "LABEL.USERNAME",
    "LABEL.PASSWORD",
    "LABEL.PERSIST",
    "LABEL.OPTIONS",
    "LABEL.PRIORITY",
    "LABEL.MAX_CONNECTIONS",
    "LABEL.METHOD",
    "LABEL.EPG",
    "LABEL.HEADERS",
    "LABEL.CHILD",
    "LABEL.EXP_DATE",
];

#[derive(Clone, PartialEq)]
pub enum InputRow {
    Input(Rc<ConfigInputDto>),
    Alias(Rc<ConfigInputAliasDto>, Rc<ConfigInputDto>),
}

#[derive(Properties, PartialEq, Clone)]
pub struct InputTableProps {
    pub inputs: Option<Vec<Rc<InputRow>>>,
}

#[component]
pub fn InputTable(props: &InputTableProps) -> Html {
    let translate = use_translation();

    let render_header_cell = make_translated_header_callback(translate.clone(), &HEADERS);

    let render_data_cell = {
        let translator = translate.clone();
        Callback::<(usize, usize, Rc<InputRow>), Html>::from(move |(_row, col, input): (usize, usize, Rc<InputRow>)| {
            match &*input {
                InputRow::Input(dto) => match col {
                    0 => html! { <Chip class={ convert_bool_to_chip_style(dto.enabled) }
                    label={if dto.enabled {translator.t("LABEL.ACTIVE")} else { translator.t("LABEL.DISABLED")} }
                     /> },
                    1 => html! { dto.name.as_ref() },
                    2 => html! { <InputTypeView input_type={dto.input_type}/> },
                    3 => html! { if dto.input_type.is_batch() {
                        <RevealContent preview={html!{dto.url.as_str()}}><BatchInputContentView input={ dto.clone() } /></RevealContent>
                        } else {
                          {dto.url.as_str()}
                        }
                    },
                    4 => dto.username.as_ref().map_or_else(|| html! {}, |u| html! {u}),
                    5 => dto
                        .password
                        .as_ref()
                        .map_or_else(|| html! {}, |pwd| html! { <HideContent content={pwd.to_string()}></HideContent>}),
                    6 => dto.persist.as_ref().map_or_else(|| html! {}, |p| html! {p}),
                    7 => {
                        html! { <RevealContent preview={ html!{translator.t("LABEL.SETTINGS")}}><InputOptions input={dto.clone()} /></RevealContent> }
                    }
                    8 => html! { dto.priority.to_string() },
                    9 => html! { dto.max_connections.to_string() },
                    10 => html! { dto.method.to_string() },
                    11 => html_if!(dto.epg.is_some(),
                                 { <RevealContent preview={ html!{ dto.epg.as_ref().map_or_else(|| html!{}, |e| html! {
                                      <Chip class={if e.smart_match.is_some() {"active"} else { "" }}
                                       label={ if e.smart_match.is_some() {translator.t("LABEL.SMART_EPG")} else { translator.t("LABEL.DEFAULT_EPG")}}
                                       />
                                   })}}>
                                      <EpgConfigView epg={ dto.epg.clone() } />
                                   </RevealContent> }),
                    12 => {
                        html! { <RevealContent preview={ html!{ dto.headers.iter().next().map_or_else(String::new, |(key, value)| format!("{key}: {value}")) } }>
                            <InputHeaders headers={dto.headers.clone()} />
                        </RevealContent> }
                    }
                    13 => dto.child.as_ref().map_or_else(|| html! {}, |c| html! { c.as_ref() }),
                    14 => dto
                        .exp_date
                        .as_ref()
                        .and_then(|ts| unix_ts_to_str(*ts))
                        .map(|s| html! { { s } })
                        .unwrap_or_else(|| html! { <AppIcon name="Unlimited" /> }),
                    _ => html! {""},
                },
                InputRow::Alias(alias, _dto) => match col {
                    0 => html! {
                        <Chip class={format!("{} tp__input-table__alias", convert_bool_to_chip_style(alias.enabled).map_or("alias", |s| if s == "active" { "alias" } else {"inactive"} )) }
                         label={if alias.enabled {translator.t("LABEL.ALIAS")} else { translator.t("LABEL.DISABLED")} }
                          />
                    },
                    1 => html! { alias.name.as_ref() },
                    3 => html! { alias.url.as_str() },
                    4 => alias.username.as_ref().map_or_else(|| html! {}, |u| html! {u}),
                    5 => alias
                        .password
                        .as_ref()
                        .map_or_else(|| html! {}, |pwd| html! { <HideContent content={pwd.to_string()}></HideContent>}),
                    8 => html! { alias.priority.to_string() },
                    9 => html! { alias.max_connections.to_string() },
                    14 => alias
                        .exp_date
                        .as_ref()
                        .and_then(|ts| unix_ts_to_str(*ts))
                        .map(|s| html! { { s } })
                        .unwrap_or_else(|| html! { <AppIcon name="Unlimited" /> }),
                    _ => html! {},
                },
            }
        })
    };

    let is_sortable = Callback::<usize, bool>::from(move |_col| false);

    let on_sort = Callback::<Option<(usize, SortOrder)>, ()>::from(move |_args| {});

    let table_definition = {
        let render_header_cell_cb = render_header_cell.clone();
        let render_data_cell_cb = render_data_cell.clone();
        let is_sortable = is_sortable.clone();
        let on_sort = on_sort.clone();
        let num_cols = HEADERS.len();
        use_memo(props.inputs.clone(), |inputs| {
            inputs.as_ref().map(|list| {
                Rc::new(TableDefinition::<InputRow> {
                    items: if list.is_empty() { None } else { Some(Rc::new(list.clone())) },
                    num_cols,
                    is_sortable,
                    on_sort,
                    render_header_cell: render_header_cell_cb,
                    render_data_cell: render_data_cell_cb,
                })
            })
        })
    };

    html! {
        <div class="tp__input-table">
          {
              if let Some(definition) = table_definition.as_ref() {
                html! {
                     <Table::<InputRow> definition={definition.clone()} />
                  }
              } else {
                  html! {}
              }
          }
        </div>
    }
}
