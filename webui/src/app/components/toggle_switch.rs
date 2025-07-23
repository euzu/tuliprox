use yew::prelude::*;

#[derive(Properties, Clone, PartialEq, Debug)]
pub struct ToggleSwitchProps {
    #[prop_or_default]
    pub value: bool,
    #[prop_or_default]
    pub readonly: bool,
}

#[function_component]
pub fn ToggleSwitch(props: &ToggleSwitchProps) -> Html {
    let toggled = use_state(|| props.value);

    let onclick = {
        let toggled = toggled.clone();
        let readonly = props.readonly;
        Callback::from(move |e: MouseEvent|  {
            if readonly {
                e.prevent_default();
                return;
            }
            toggled.set(!*toggled)
        })
    };

    html! {
        <label class={classes!("tp__toggle-switch", if props.readonly { "tp__toggle-switch__readonly" } else {""})}>
            <input type="checkbox"
                   checked={*toggled}
                   onclick={onclick}/>
              <span class={classes!("tp__toggle-switch__track", if *toggled { "tp__toggle-switch__active" } else { "" })}>
               <span class={classes!("tp__toggle-switch__toggle", if *toggled { "tp__toggle-switch__on" } else { "" })}>
              </span>
            </span>
        </label>
    }
}
