use crate::app::components::{date_input::DateInputBase, IconButton};
use web_sys::MouseEvent;
use yew::{component, html, Callback, Html, NodeRef, Properties};

#[derive(Clone, PartialEq, Debug)]
pub struct ToolAction {
    pub name: Option<String>,
    pub icon: String,
    pub hint: Option<String>,
    pub class: Option<String>,
    pub onclick: Callback<MouseEvent>,
}

#[derive(Properties, Clone, PartialEq, Debug)]
pub struct DateInputActionProps {
    #[prop_or_default]
    pub name: String,
    #[prop_or_default]
    pub field_id: Option<String>,
    #[prop_or_default]
    pub label: Option<String>,
    #[prop_or_default]
    pub input_ref: Option<NodeRef>,
    #[prop_or_default]
    pub value: Option<i64>,
    #[prop_or_default]
    pub on_change: Option<Callback<Option<i64>>>,
    #[prop_or_default]
    pub tool_action: Option<ToolAction>,
}

#[component]
pub fn DateInputAction(props: &DateInputActionProps) -> Html {
    let handle_tool_click = {
        let tool_action = props.tool_action.clone();
        Callback::from(move |(_, event): (String, MouseEvent)| {
            if let Some(action) = tool_action.as_ref() {
                action.onclick.emit(event);
            }
        })
    };

    let tools = if let Some(action) = props.tool_action.as_ref() {
        html! {
            <div class="tp__input-tools">
                <IconButton
                    class={if let Some(tool_class) = action.class.as_ref() {
                        format!("tp__input-tool {tool_class}")
                    } else {
                        "tp__input-tool".to_string()
                    }}
                    name={action.name.clone().unwrap_or_else(|| props.name.clone())}
                    icon={action.icon.clone()}
                    hint={action.hint.clone().unwrap_or_default()}
                    onclick={handle_tool_click}
                />
            </div>
        }
    } else {
        Html::default()
    };

    html! {
        <DateInputBase
            name={props.name.clone()}
            field_id={props.field_id.clone()}
            label={props.label.clone()}
            input_ref={props.input_ref.clone()}
            value={props.value}
            on_change={props.on_change.clone()}
            tools={tools}
            extra_class={props.tool_action.is_some().then_some("tp__input-date-tools")}
        />
    }
}
