use std::fmt::Display;
use crate::app::components::popup_menu::PopupMenu;
use crate::app::components::{convert_bool_to_chip_style, AppIcon, Chip, HideContent, InputHeaders, InputOptions, InputTypeView, RevealContent, Table, TableDefinition};
use std::rc::Rc;
use std::str::FromStr;
use yew::platform::spawn_local;
use yew::prelude::*;
use yew_i18n::use_translation;
use shared::error::{create_tuliprox_error_result, TuliproxError, TuliproxErrorKind};
use shared::model::{ConfigInputAliasDto, ConfigInputDto};
use crate::app::components::menu_item::MenuItem;
use crate::model::DialogResult;
use crate::services::{DialogService};

const HEADERS: [&str; 14] = [
"TABLE.EMPTY",
"TABLE.ENABLED",
"TABLE.NAME",
"TABLE.INPUT_TYPE",
"TABLE.URL",
"TABLE.USERNAME",
"TABLE.PASSWORD",
"TABLE.PERSIST",
"TABLE.OPTIONS",
"TABLE.PRIORITY",
"TABLE.MAX_CONNECTIONS",
"TABLE.METHOD",
"TABLE.EPG",
"TABLE.HEADERS",
];

#[derive(Clone, PartialEq)]
pub enum InputRow {
    Input(Rc<ConfigInputDto>),
    Alias(Rc<ConfigInputAliasDto>, Rc<ConfigInputDto>)
}


#[derive(Properties, PartialEq, Clone)]
pub struct InputTableProps {
    pub inputs: Option<Vec<Rc<InputRow>>>,
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
                            2 => html! { dto.name.as_str() },
                            3 => html! { <InputTypeView input_type={dto.input_type}/> },
                            4 => html! { <RevealContent preview={html!{dto.url.as_str()}}>{ dto.url.as_str() }</RevealContent> },
                            5 => dto.username.as_ref().map_or_else(|| html!{}, |u| html!{u}),
                            6 => dto.password.as_ref().map_or_else(|| html!{}, |pwd| html! { <HideContent content={pwd.to_string()}></HideContent>}),
                            7 => dto.persist.as_ref().map_or_else(|| html!{}, |p| html!{p}),
                            8 => html! { <InputOptions input={dto.clone()} /> },
                            9 => html! { dto.priority.to_string() },
                            10 => html! { dto.max_connections.to_string() },
                            11 => html! { dto.method.to_string() },
                            12 => html! {  },
                            13 => html! { <InputHeaders input={dto.clone()} /> },
                            _ => html! {""},
                        }
                    },
                    InputRow::Alias(alias, dto) => {
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
                                <Chip class={ format!("{} tp__input-table__alias", convert_bool_to_chip_style(dto.enabled).map_or_else(String::new, |s| if s == "active" { "alias".to_string() } else {s} )) }
                                 label={translator.t("LABEL.ALIAS")}  />
                            },
                            2 => html! { alias.name.as_str() },
                            3 => html! { },
                            4 => html! { alias.url.as_str() },
                            5 => alias.username.as_ref().map_or_else(|| html!{}, |u| html!{u}),
                            6 => alias.password.as_ref().map_or_else(|| html!{}, |pwd| html! { <HideContent content={pwd.to_string()}></HideContent>}),
                            7 => html! { },
                            8 => html! { },
                            9 => html! { alias.priority.to_string() },
                            10 => html! { alias.max_connections.to_string() },
                            11 => html! { },
                            12 => html! { },
                            13 => html! { },
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
        let selected_dto = selected_dto.clone();
        Callback::from(move |name:String| {
            if let Ok(action) = TableAction::from_str(&name) {
                match action {
                    TableAction::Edit => {}
                    TableAction::Refresh => {}
                    TableAction::Delete => {
                        let confirm = confirm.clone();
                        let translator = translate.clone();
                        spawn_local(async move {
                            let result = confirm.confirm(&translator.t("MESSAGES.CONFIRM_DELETE")).await;
                            if result == DialogResult::Ok {
                                // TODO edit
                            }
                        });
                    }
                }
            }
            popup_is_open_state.set(false);
        })
    };

    let table_definition = {
        let render_header_cell_cb = render_header_cell.clone();
        let render_data_cell_cb = render_data_cell.clone();
        let num_cols = HEADERS.len();
        use_memo(props.inputs.clone(), |inputs|
            inputs.as_ref().map(|list|
                Rc::new(TableDefinition::<InputRow> {
                    items: Rc::new(list.clone()),
                    num_cols,
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
                            <MenuItem icon="Delete" name={TableAction::Delete.to_string()} label={translate.t("LABEL.DELETE")} onclick={&handle_menu_click} style="tp__delete_action"></MenuItem>
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
            create_tuliprox_error_result!(TuliproxErrorKind::Info, "Unknown InputType: {}", s)
        }
    }
}