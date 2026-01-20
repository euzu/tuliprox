use yew::prelude::*;

#[derive(Properties, Clone, PartialEq, Debug)]
pub struct ToggleSwitchProps {
    #[prop_or_default]
    pub value: bool,
    #[prop_or_default]
    pub readonly: bool,
    #[prop_or_else(Callback::noop)]
    pub on_change: Callback<bool>,
}

#[function_component]
pub fn ToggleSwitch(props: &ToggleSwitchProps) -> Html {
    let toggled = use_state(|| props.value);

    {
        let toggled = toggled.clone();
        use_effect_with(props.value, move |value| {
            toggled.set(*value);
            || ()
        });
    }

    let onclick = {
        let toggled = toggled.clone();
        let readonly = props.readonly;
        let onchange = props.on_change.clone();
        Callback::from(move |e: MouseEvent|  {
            if readonly {
                e.prevent_default();
                e.stop_propagation();
                return;
            }
            let new_value = !*toggled;
            toggled.set(new_value);
            onchange.emit(new_value);
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
