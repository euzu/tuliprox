use std::fmt::Display;
use crate::app::components::popup_menu::PopupMenu;
use crate::app::components::{convert_bool_to_chip_style, AppIcon, BatchInputContentView, Chip, EpgConfigView, HideContent, InputHeaders, InputOptions, InputTypeView, RevealContent, StagedInputView, Table, TableDefinition};
use std::rc::Rc;
use std::str::FromStr;
use yew::platform::spawn_local;
use yew::prelude::*;
use yew_i18n::use_translation;
use shared::concat_string;
use shared::error::{info_err_res, TuliproxError};
use shared::model::{ConfigInputAliasDto, ConfigInputDto, SortOrder};
use crate::app::components::menu_item::MenuItem;
use crate::html_if;
use crate::model::DialogResult;
use crate::services::{DialogService};
use shared::model::InputType;
use shared::utils::unix_ts_to_str;

const HEADERS: [&str; 16] = [
"LABEL.EMPTY",
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
"LABEL.STAGED",
"LABEL.EXP_DATE",
];

#[derive(Clone, PartialEq)]
pub enum InputRow {
    Input(Rc<ConfigInputDto>),
    Alias(Rc<ConfigInputAliasDto>, Rc<ConfigInputDto>)
}


#[derive(Properties, PartialEq, Clone)]
pub struct InputTableProps {
    pub inputs: Option<Vec<Rc<InputRow>>>,
    #[prop_or_default]
    pub on_edit: Option<Callback<Rc<ConfigInputDto>>>,
    #[prop_or_default]
    pub on_delete: Option<Callback<String>>,
}

#[function_component]
pub fn InputTable(props: &InputTableProps) -> Html {
    let translate = use_translation();
    let dialog = use_context::<DialogService>().expect("Dialog service not found");
    let popup_anchor_ref = use_state(|| None::<web_sys::Element>);
    let popup_is_open = use_state(|| false);
    let selected_dto = use_state(|| None::<Rc<InputRow>>);

    let handle_popup_close = {
        let set_is_open = popup_is_open.clone();
        Callback::from(move |()| {
            set_is_open.set(false);
        })
    };

    let handle_popup_onclick = {
        let set_selected_dto = selected_dto.clone();
        let set_anchor_ref = popup_anchor_ref.clone();
        let set_is_open = popup_is_open.clone();
        Callback::from(move |(dto, event): (Rc<InputRow>, MouseEvent)| {
            if let Some(target) = event.target_dyn_into::<web_sys::Element>() {
                set_selected_dto.set(Some(dto.clone()));
                set_anchor_ref.set(Some(target));
                set_is_open.set(true);
            }
        })
    };

    let render_header_cell = {
        let translator = translate.clone();
        Callback::<usize, Html>::from(move |col| {
            html! {
                {
                    if col < HEADERS.len() {
                       translator.t(HEADERS[col])
                    } else {
                      String::new()
                    }
               }
            }
        })
    };

    let render_data_cell = {
        let translator = translate.clone();
        let popup_onclick = handle_popup_onclick.clone();
        Callback::<(usize, usize, Rc<InputRow>), Html>::from(
            move |(row, col, input): (usize, usize, Rc<InputRow>)| {
                match &*input {
                    InputRow::Input(dto) => {
                        match col {
                            0 => {
                                let popup_onclick = popup_onclick.clone();
                                html! {
                            <button class="tp__icon-button"
                                onclick={Callback::from(move |event: MouseEvent| popup_onclick.emit((input.clone(), event)))}
                                data-row={row.to_string()}>
                                <AppIcon name="Popup"></AppIcon>
                            </button>
                        }
                            }
                            1 => html! { <Chip class={ convert_bool_to_chip_style(dto.enabled) }
                                 label={if dto.enabled {translator.t("LABEL.ACTIVE")} else { translator.t("LABEL.DISABLED")} }
                                  /> },
                            2 => html! { dto.name.as_ref() },
                            3 => html! { <InputTypeView input_type={dto.input_type}/> },
                            4 => html! { if matches!(dto.input_type, InputType::XtreamBatch | InputType::M3uBatch) {
                                <RevealContent preview={html!{dto.url.as_str()}}><BatchInputContentView input={ dto.clone() } /></RevealContent>
                                } else {
                                  {dto.url.as_str()}
                                }
                            },
                            5 => dto.username.as_ref().map_or_else(|| html!{}, |u| html!{u}),
                            6 => dto.password.as_ref().map_or_else(|| html!{}, |pwd| html! { <HideContent content={pwd.to_string()}></HideContent>}),
                            7 => dto.persist.as_ref().map_or_else(|| html!{}, |p| html!{p}),
                            8 => html! { <InputOptions input={dto.clone()} /> },
                            9 => html! { dto.priority.to_string() },
                            10 => html! { dto.max_connections.to_string() },
                            11 => html! { dto.method.to_string() },
                            12 => html_if!(dto.epg.is_some(),
                                 { <RevealContent preview={ html!{ dto.epg.as_ref().map_or_else(|| html!{}, |e| html! {
                                      <Chip class={if e.smart_match.is_some() {"active"} else { "" }}
                                       label={ if e.smart_match.is_some() {translator.t("LABEL.SMART_EPG")} else { translator.t("LABEL.DEFAULT_EPG")}}
                                       />
                                   })}}>
                                      <EpgConfigView epg={ dto.epg.clone() } />
                                   </RevealContent> }),
                            13 => html! { <RevealContent preview={ html!{ dto.headers.iter().next().map_or_else(String::new, |(key, value)| format!("{key}: {value}")) } }>
                                        <InputHeaders headers={dto.headers.clone()} />
                                    </RevealContent> },
                            14 => html_if!(dto.staged.is_some(),
                                 { <RevealContent preview={ html!{ dto.staged.as_ref().map_or_else(String::new, |s| s.url.clone())} }>
                                      <StagedInputView input={ dto.staged.clone() } />
                                   </RevealContent> }),
                            15 => dto.exp_date.as_ref().and_then(|ts| unix_ts_to_str(*ts))
                                    .map(|s| html! { { s } }).unwrap_or_else(|| html! { <AppIcon name="Unlimited" /> }),
                            _ => html! {""},
                        }
                    },
                    InputRow::Alias(alias, _dto) => {
                        match col {
                            0 => {
                                let popup_onclick = popup_onclick.clone();
                                html! {
                                    <button class="tp__icon-button"
                                        onclick={Callback::from(move |event: MouseEvent| popup_onclick.emit((input.clone(), event)))}
                                        data-row={row.to_string()}>
                                        <AppIcon name="Popup"></AppIcon>
                                    </button>
                                }
                            }
                            1 => html! {
                                <Chip class={format!("{} tp__input-table__alias", convert_bool_to_chip_style(alias.enabled).map_or("alias", |s| if s == "active" { "alias" } else {"inactive"} )) }
                                 label={if alias.enabled {translator.t("LABEL.ALIAS")} else { translator.t("LABEL.DISABLED")} }
                                  />
                            },
                            2 => html! { alias.name.as_ref() },
                            4 => html! { alias.url.as_str() },
                            5 => alias.username.as_ref().map_or_else(|| html!{}, |u| html!{u}),
                            6 => alias.password.as_ref().map_or_else(|| html!{}, |pwd| html! { <HideContent content={pwd.to_string()}></HideContent>}),
                            9 => html! { alias.priority.to_string() },
                            10 => html! { alias.max_connections.to_string() },
                            15 => alias.exp_date.as_ref().and_then(|ts| unix_ts_to_str(*ts))
                                .map(|s| html! { { s } }).unwrap_or_else(|| html! { <AppIcon name="Unlimited" /> }),
                            _ => html! { },
                        }
                    }
                }
            })
    };


    let handle_menu_click = {
        let popup_is_open_state = popup_is_open.clone();
        let confirm = dialog.clone();
        let translate = translate.clone();
        let on_edit = props.on_edit.clone();
        let on_delete = props.on_delete.clone();
        let selected_dto = selected_dto.clone();

        Callback::from(move |(name, _): (String, _)| {
            if let Ok(action) = TableAction::from_str(&name) {
                match action {
                    TableAction::Edit => {
                        if let (Some(on_edit), Some(selected)) = (&on_edit, &*selected_dto) {
                            if let InputRow::Input(dto) = &**selected {
                                on_edit.emit(dto.clone());
                            }
                        }
                    }
                    TableAction::Refresh => {}
                    TableAction::Delete => {
                        let confirm = confirm.clone();
                        let translator = translate.clone();
                        let on_delete = on_delete.clone();
                        let selected_dto = selected_dto.clone();
                        spawn_local(async move {
                            let result = confirm.confirm(&translator.t("MESSAGES.CONFIRM_DELETE")).await;
                            if result == DialogResult::Ok {
                                if let (Some(on_delete), Some(selected)) = (on_delete, &*selected_dto) {
                                    if let InputRow::Input(dto) = &**selected {
                                        on_delete.emit(dto.name.to_string());
                                    }
                                }
                            }
                        });
                    }
                }
            }
            popup_is_open_state.set(false);
        })
    };

    let is_sortable = Callback::<usize, bool>::from(move |_col| {
        false
    });

    let on_sort = Callback::<Option<(usize, SortOrder)>, ()>::from(move |_args| {
    });

    let table_definition = {
        let render_header_cell_cb = render_header_cell.clone();
        let render_data_cell_cb = render_data_cell.clone();
        let is_sortable = is_sortable.clone();
        let on_sort = on_sort.clone();
        let num_cols = HEADERS.len();
        use_memo(props.inputs.clone(), |inputs|
            inputs.as_ref().map(|list|
                Rc::new(TableDefinition::<InputRow> {
                    items: if list.is_empty() {None} else {Some(Rc::new(list.clone()))},
                    num_cols,
                    is_sortable,
                    on_sort,
                    render_header_cell: render_header_cell_cb,
                    render_data_cell: render_data_cell_cb,
                }))
        )
    };

    html! {
        <div class="tp__input-table">
          {
              if let Some(definition) = table_definition.as_ref() {
                html! {
                    <>
                       <Table::<InputRow> definition={definition.clone()} />
                        <PopupMenu is_open={*popup_is_open} anchor_ref={(*popup_anchor_ref).clone()} on_close={handle_popup_close}>
                            <MenuItem icon="Edit" name={TableAction::Edit.to_string()} label={translate.t("LABEL.EDIT")} onclick={&handle_menu_click}></MenuItem>
                            <MenuItem icon="Delete" name={TableAction::Delete.to_string()} label={translate.t("LABEL.DELETE")} onclick={&handle_menu_click} class="tp__delete_action"></MenuItem>
                        </PopupMenu>
                    </>
                  }
              } else {
                  html! {}
              }
          }
        </div>
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
enum TableAction {
    Edit,
    Refresh,
    Delete,
}

impl Display for TableAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", match self {
            Self::Edit => "edit",
            Self::Refresh => "refresh",
            Self::Delete => "delete",
        })
    }
}

impl FromStr for TableAction {
    type Err = TuliproxError;

    fn from_str(s: &str) -> Result<Self, TuliproxError> {
        if s.eq("edit") {
            Ok(Self::Edit)
        } else if s.eq("refresh") {
                Ok(Self::Refresh)
        } else if s.eq("delete") {
            Ok(Self::Delete)
        } else {
            info_err_res!("Unknown TableAction: {}", s)
        }
    }
}