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

crate::app::components::define_table_columns! {
    enum InputColumn {
        Enabled => ("enabled", "LABEL.ENABLED"),
        Name => ("name", "LABEL.NAME") { can_hide: false },
        InputType => ("input_type", "LABEL.INPUT_TYPE"),
        Url => ("url", "LABEL.URL"),
        Username => ("username", "LABEL.USERNAME"),
        Password => ("password", "LABEL.PASSWORD"),
        Persist => ("persist", "LABEL.PERSIST"),
        Options => ("options", "LABEL.OPTIONS"),
        Priority => ("priority", "LABEL.PRIORITY"),
        MaxConnections => ("max_connections", "LABEL.MAX_CONNECTIONS"),
        Method => ("method", "LABEL.METHOD"),
        Epg => ("epg", "LABEL.EPG"),
        Headers => ("headers", "LABEL.HEADERS"),
        Provider => ("provider", "LABEL.PROVIDER"),
        ExpDate => ("exp_date", "LABEL.EXP_DATE"),
    }
}

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
    let columns = use_memo((), |()| InputColumn::columns());

    let render_header_cell = make_translated_header_callback(translate.clone(), |index| {
        InputColumn::from_index(index).map(InputColumn::header_label)
    });

    let render_data_cell = {
        let translator = translate.clone();
        Callback::<(usize, usize, Rc<InputRow>), Html>::from(move |(_row, col, input): (usize, usize, Rc<InputRow>)| {
            match &*input {
                InputRow::Input(dto) => match InputColumn::from_index(col) {
                    Some(InputColumn::Enabled) => html! { <Chip class={ convert_bool_to_chip_style(dto.enabled) }
                    label={if dto.enabled {translator.t("LABEL.ACTIVE")} else { translator.t("LABEL.DISABLED")} }
                     /> },
                    Some(InputColumn::Name) => html! { dto.name.as_ref() },
                    Some(InputColumn::InputType) => html! { <InputTypeView input_type={dto.input_type}/> },
                    Some(InputColumn::Url) => html! { if dto.input_type.is_batch() {
                        <RevealContent preview={html!{dto.url.as_str()}}><BatchInputContentView input={ dto.clone() } /></RevealContent>
                        } else {
                          {dto.url.as_str()}
                        }
                    },
                    Some(InputColumn::Username) => dto.username.as_ref().map_or_else(|| html! {}, |u| html! {u}),
                    Some(InputColumn::Password) => dto
                        .password
                        .as_ref()
                        .map_or_else(|| html! {}, |pwd| html! { <HideContent content={pwd.clone()}></HideContent>}),
                    Some(InputColumn::Persist) => dto.persist.as_ref().map_or_else(|| html! {}, |p| html! {p}),
                    Some(InputColumn::Options) => {
                        html! { <RevealContent preview={ html!{translator.t("LABEL.SETTINGS")}}><InputOptions input={dto.clone()} /></RevealContent> }
                    }
                    Some(InputColumn::Priority) => html_if!(!dto.input_type.is_staged(), { dto.priority.to_string() }),
                    Some(InputColumn::MaxConnections) => {
                        html_if!(!dto.input_type.is_staged(), { dto.max_connections.to_string() })
                    }
                    Some(InputColumn::Method) => html! { dto.method.to_string() },
                    Some(InputColumn::Epg) => html_if!(dto.epg.is_some(),
                                 { <RevealContent preview={ html!{ dto.epg.as_ref().map_or_else(|| html!{}, |e| html! {
                                      <Chip class={if e.smart_match.is_some() {"active"} else { "" }}
                                       label={ if e.smart_match.is_some() {translator.t("LABEL.SMART_EPG")} else { translator.t("LABEL.DEFAULT_EPG")}}
                                       />
                                   })}}>
                                      <EpgConfigView epg={ dto.epg.clone() } />
                                   </RevealContent> }),
                    Some(InputColumn::Headers) => {
                        html! { <RevealContent preview={ html!{ dto.headers.iter().next().map_or_else(String::new, |(key, value)| format!("{key}: {value}")) } }>
                            <InputHeaders headers={dto.headers.clone()} />
                        </RevealContent> }
                    }
                    Some(InputColumn::Provider) => dto
                        .staged
                        .as_ref()
                        .and_then(|staged| staged.for_input.as_ref())
                        .map_or_else(|| html! {}, |provider| html! { provider.as_ref() }),
                    Some(InputColumn::ExpDate) => dto
                        .exp_date
                        .as_ref()
                        .and_then(|ts| unix_ts_to_str(*ts))
                        .map_or_else(|| html! { <AppIcon name="Unlimited" /> }, |s| html! { { s } }),
                    _ => html! {""},
                },
                InputRow::Alias(alias, _dto) => match InputColumn::from_index(col) {
                    Some(InputColumn::Enabled) => html! {
                        <Chip class={format!("{} tp__input-table__alias", convert_bool_to_chip_style(alias.enabled).map_or("alias", |s| if s == "active" { "alias" } else {"inactive"} )) }
                         label={if alias.enabled {translator.t("LABEL.ALIAS")} else { translator.t("LABEL.DISABLED")} }
                          />
                    },
                    Some(InputColumn::Name) => html! { alias.name.as_ref() },
                    Some(InputColumn::Url) => html! { alias.url.as_str() },
                    Some(InputColumn::Username) => alias.username.as_ref().map_or_else(|| html! {}, |u| html! {u}),
                    Some(InputColumn::Password) => alias
                        .password
                        .as_ref()
                        .map_or_else(|| html! {}, |pwd| html! { <HideContent content={pwd.clone()}></HideContent>}),
                    Some(InputColumn::Priority) => html! { alias.priority.to_string() },
                    Some(InputColumn::MaxConnections) => html! { alias.max_connections.to_string() },
                    Some(InputColumn::ExpDate) => alias
                        .exp_date
                        .as_ref()
                        .and_then(|ts| unix_ts_to_str(*ts))
                        .map_or_else(|| html! { <AppIcon name="Unlimited" /> }, |s| html! { { s } }),
                    _ => html! {},
                },
            }
        })
    };

    let on_sort = Callback::<Option<(usize, SortOrder)>, ()>::from(move |_args| {});

    let table_definition = {
        let columns = columns.clone();
        let render_header_cell_cb = render_header_cell.clone();
        let render_data_cell_cb = render_data_cell.clone();

        let on_sort = on_sort.clone();

        use_memo(props.inputs.clone(), |inputs| {
            let list = inputs.as_deref().unwrap_or_default();
            {
                Rc::new(TableDefinition::<InputRow> {
                    table_id: "playlist.inputs".into(),
                    columns: columns.clone(),
                    row_key: Callback::from(|(_, item): (usize, Rc<InputRow>)| match item.as_ref() {
                        InputRow::Input(input) => format!("input:{}", input.id).into(),
                        InputRow::Alias(alias, _) => format!("alias:{}", alias.id).into(),
                    }),
                    items: if list.is_empty() { None } else { Some(Rc::new(list.to_vec())) },

                    on_sort,
                    render_header_cell: render_header_cell_cb,
                    render_data_cell: render_data_cell_cb,
                })
            }
        })
    };

    html! {
        <div class="tp__input-table">
          {
              html! { <Table::<InputRow> definition={(*table_definition).clone()} /> }
          }
        </div>
    }
}

#[cfg(test)]
#[path = "input_table.test.rs"]
mod tests;
