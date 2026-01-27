#[macro_export]
macro_rules! config_field_optional {
    ($config:expr, $label:expr, $field:ident) => {
        html! {
            <div class="tp__form-field tp__form-field__text">
                <label>{$label}</label>
                <span class="tp__form-field__value">
                {
                    match $config.$field.as_ref() {
                        Some(value) => html! { &value },
                        None => Html::default(),
                    }
                 }
                </span>
            </div>
        }
    };
}

#[macro_export]
macro_rules! config_field_optional_hide {
    ($config:expr, $label:expr, $field:ident) => {
        html! {
            <div class="tp__form-field tp__form-field__text">
                <label>{$label}</label>
                <span class="tp__form-field__value">
                    <$crate::app::components::HideContent content={$config.$field.as_ref().map_or_else(String::new, |content| content.clone())}></$crate::app::components::HideContent>
                </span>
            </div>
        }
    };
}

#[macro_export]
macro_rules! config_field_hide {
    ($config:expr, $label:expr, $field:ident) => {
        html! {
            <div class="tp__form-field tp__form-field__text">
                <label>{$label}</label>
                <span class="tp__form-field__value">
                    <$crate::app::components::HideContent content={$config.$field.clone()}></$crate::app::components::HideContent>
                </span>
            </div>
        }
    };
}

#[macro_export]
macro_rules! config_field_bool {
    ($config:expr, $label:expr, $field:ident) => {
        html! {
            <div class="tp__form-field tp__form-field__bool">
                <$crate::app::components::ToggleSwitch value={$config.$field} readonly={true} />
                <label>{$label}</label>
            </div>
        }
    };
}

#[macro_export]
macro_rules! config_field_bool_empty {
    ($label:expr) => {
        html! {
            <div class="tp__form-field tp__form-field__bool">
                <label>{$label}</label>
                <$crate::app::components::ToggleSwitch value={false} readonly={true} />
            </div>
        }
    };
}

#[macro_export]
macro_rules! config_field {
    ($config:expr, $label:expr, $field:ident) => {
        html! {
            <div class="tp__form-field tp__form-field__text">
                <label>{$label}</label>
                <span class="tp__form-field__value">{$config.$field.to_string()}</span>
            </div>
        }
    };
}

#[macro_export]
macro_rules! config_field_custom {
    ($label:expr, $value:expr) => {
        html! {
            <div class="tp__form-field tp__form-field__text">
                <label>{$label}</label>
                <span class="tp__form-field__value">{$value}</span>
            </div>
        }
    };
}

#[macro_export]
macro_rules! config_field_custom_hide {
    ($label:expr, $value:expr) => {
        html! {
            <div class="tp__form-field tp__form-field__text">
                <label>{$label}</label>
                <span class="tp__form-field__value">
                    <$crate::app::components::HideContent content={$value}></$crate::app::components::HideContent>
                </span>
            </div>
        }
    };
}

#[macro_export]
macro_rules! config_field_child {
    ($label:expr, $body:block) => {
        html! {
            <div class="tp__form-field tp__form-field__text">
                <label>{$label}</label>
                { $body }
            </div>
        }
    };
}

#[macro_export]
macro_rules! config_field_empty {
    ($label:expr) => {
        html! {
            <div class="tp__form-field tp__form-field__text">
                <label>{$label}</label>
                <span class="tp__form-field__value"></span>
            </div>
        }
    };
}

pub trait HasFormData {
    type Data;
    fn data(&self) -> &Self::Data;
    //fn data_mut(&mut self) -> &mut Self::Data;
}

#[macro_export]
macro_rules! edit_field_text_option {
    ($instance:expr, $label:expr, $field:ident, $action:path) => {
        $crate::edit_field_text_option!(@inner $instance, $label, $field, $action, false)
    };
    ($instance:expr, $label:expr, $field:ident, $action:path, $hidden:expr) => {
        $crate::edit_field_text_option!(@inner $instance, $label, $field, $action, $hidden)
    };
    (@inner $instance:expr, $label:expr, $field:ident, $action:path, $hidden:expr) => {{
        let instance = $instance.clone();
        html! {
            <div class="tp__form-field tp__form-field__text">
                <$crate::app::components::input::Input
                    label={$label}
                    hidden={$hidden}
                    name={stringify!($field)}
                    autocomplete={true}
                    value={instance.form.$field.as_ref().map_or_else(String::new, |v|v.to_string())}
                    on_change={Callback::from(move |value: String| {
                        instance.dispatch($action(if value.is_empty() {
                            None
                        } else {
                            Some(value)
                        }));
                    })}
                />
            </div>
        }
    }};
}

#[macro_export]
macro_rules! edit_field_textarea_option {
    ($instance:expr, $label:expr, $field:ident, $action:path) => {{
        let instance = $instance.clone();
        html! {
            <div class="tp__form-field tp__form-field__textarea">
                <$crate::app::components::TextArea
                    label={$label}
                    name={stringify!($field)}
                    value={instance.form.$field.as_ref().map_or_else(String::new, |v|v.to_string())}
                    on_change={Callback::from(move |value: String| {
                        instance.dispatch($action(if value.is_empty() {
                            None
                        } else {
                            Some(value)
                        }));
                    })}
                />
            </div>
        }
    }};
}

#[macro_export]
macro_rules! edit_field_text {
    ($instance:expr, $label:expr, $field:ident, $action:path) => {
        $crate::edit_field_text!(@inner $instance, $label, $field, $action, false)
    };
    ($instance:expr, $label:expr, $field:ident, $action:path,  $hidden:expr) => {
        $crate::edit_field_text!(@inner $instance, $label, $field, $action, $hidden)
    };
    (@inner $instance:expr, $label:expr, $field:ident, $action:path, $hidden:expr) => {{
        let instance = $instance.clone();
        html! {
            <div class="tp__form-field tp__form-field__text">
                <$crate::app::components::input::Input
                    label={$label}
                    hidden={$hidden}
                    name={stringify!($field)}
                    autocomplete={true}
                    value={instance.form.$field.to_string()}
                    on_change={Callback::from(move |value: String| {
                        instance.dispatch($action(value.into()));
                    })}
                />
            </div>
        }
    }};
}

#[macro_export]
macro_rules! edit_field_bool {
    ($instance:expr, $label:expr, $field:ident, $action:path) => {{
        let instance = $instance.clone();
        html! {
            <div class="tp__form-field tp__form-field__bool">
                <$crate::app::components::ToggleSwitch
                     value={instance.form.$field}
                     readonly={false}
                     on_change={Callback::from(move |value| instance.dispatch($action(value)))} />
                <label>{$label}</label>
            </div>
        }
    }};
}

#[macro_export]
macro_rules! edit_field_number {
    ($instance:expr, $label:expr, $field:ident, $action:path) => {{
        let instance = $instance.clone();
        html! {
            <div class="tp__form-field tp__form-field__number">
                <$crate::app::components::number_input::NumberInput
                    label={$label}
                    name={stringify!($field)}
                    value={instance.form.$field as i64}
                    on_change={Callback::from(move |value: Option<i64>| {
                        match value {
                            Some(value) => match u32::try_from(value) {
                                Ok(val) => instance.dispatch($action(val)),
                                Err(_) => return,
                            },
                            None => instance.dispatch($action(0)),
                        }
                    })}
                />
            </div>
        }
    }};
}

#[macro_export]
macro_rules! edit_field_number_u8 {
    ($instance:expr, $label:expr, $field:ident, $action:path) => {{
        let instance = $instance.clone();
        html! {
            <div class="tp__form-field tp__form-field__number">
                <$crate::app::components::number_input::NumberInput
                    label={$label}
                    name={stringify!($field)}
                    value={instance.form.$field as i64}
                    on_change={Callback::from(move |value: Option<i64>| {
                        match value {
                            Some(value) => {
                                let val = u8::try_from(value).unwrap_or(0);
                                instance.dispatch($action(val))
                            },
                            None => instance.dispatch($action(0)),
                        }
                    })}
                />
            </div>
        }
    }};
}

#[macro_export]
macro_rules! edit_field_number_f64 {
    ($instance:expr, $label:expr, $field:ident, $action:path) => {{
        let instance = $instance.clone();
        html! {
            <div class="tp__form-field tp__form-field__number">
                <$crate::app::components::number_input::NumberInput
                    label={$label}
                    name={stringify!($field)}
                    float_value={Some(instance.form.$field)}
                    on_change_float={Some({
                        let instance = instance.clone();
                        Callback::from(move |value: Option<f64>| {
                            if let Some(value) = value {
                                if value.is_finite() {
                                    instance.dispatch($action(value));
                                }
                            }
                        })
                    })}
                />
            </div>
        }
    }};
}

#[macro_export]
macro_rules! edit_field_number_u16 {
    ($instance:expr, $label:expr, $field:ident, $action:path) => {{
        let instance = $instance.clone();
        html! {
            <div class="tp__form-field tp__form-field__number">
                <$crate::app::components::number_input::NumberInput
                    label={$label}
                    name={stringify!($field)}
                    value={instance.form.$field as i64}
                    on_change={Callback::from(move |value: Option<i64>| {
                        match value {
                            Some(value) => {
                                let val = u16::try_from(value).unwrap_or(0);
                                instance.dispatch($action(val))
                            },
                            None => instance.dispatch($action(0)),
                        }
                    })}
                />
            </div>
        }
    }};
}


#[macro_export]
macro_rules! edit_field_number_i16 {
    ($instance:expr, $label:expr, $field:ident, $action:path) => {{
        let instance = $instance.clone();
        html! {
            <div class="tp__form-field tp__form-field__number">
                <$crate::app::components::number_input::NumberInput
                    label={$label}
                    name={stringify!($field)}
                    value={instance.form.$field as i64}
                    on_change={Callback::from(move |value: Option<i64>| {
                        match value {
                            Some(value) => match i16::try_from(value) {
                                Ok(val) => instance.dispatch($action(val)),
                                Err(_) => return, // keep the existing value; optionally surface a validation error
                            },
                            None => instance.dispatch($action(0)),
                        }
                    })}
                />
            </div>
        }
    }};
}

#[macro_export]
macro_rules! edit_field_number_u64 {
    ($instance:expr, $label:expr, $field:ident, $action:path) => {{
        let instance = $instance.clone();
        html! {
            <div class="tp__form-field tp__form-field__number">
                <$crate::app::components::number_input::NumberInput
                    label={$label}
                    name={stringify!($field)}
                    value={instance.form.$field.min(i64::MAX as u64) as i64}
                    on_change={Callback::from(move |value: Option<i64>| {
                        match value {
                          Some(value) => match u64::try_from(value) {
                            Ok(val) => instance.dispatch($action(val)),
                            Err(_) => return,
                          },
                          None => instance.dispatch($action(0)),
                        }
                    })}
                />
            </div>
        }
    }};
}

#[macro_export]
macro_rules! edit_field_number_usize {
    ($instance:expr, $label:expr, $field:ident, $action:path) => {{
        let instance = $instance.clone();
        html! {
            <div class="tp__form-field tp__form-field__number">
                <$crate::app::components::number_input::NumberInput
                    label={$label}
                    name={stringify!($field)}
                    value={instance.form.$field as i64}
                    on_change={Callback::from(move |value: Option<i64>| {
                        match value {
                             Some(value) => {
                                 let val = usize::try_from(value).unwrap_or(0);
                                 instance.dispatch($action(val))
                             },
                             None => instance.dispatch($action(0)),
                        }
                    })}
                />
            </div>
        }
    }};
}

#[macro_export]
macro_rules! edit_field_number_option {
    ($instance:expr, $label:expr, $field:ident, $action:path) => {{
        let instance = $instance.clone();
        html! {
            <div class="tp__form-field tp__form-field__number">
                <$crate::app::components::number_input::NumberInput
                    label={$label}
                    name={stringify!($field)}
                    value={instance.form.$field.map(|v| v as i64)}
                    on_change={Callback::from(move |value: Option<i64>| {
                        match value {
                            Some(value) => match u32::try_from(value) {
                                Ok(val) => instance.dispatch($action(Some(val))),
                                Err(_) => return,
                            },
                            None => instance.dispatch($action(None)),
                        }
                    })}
                />
            </div>
        }
    }};
}

#[macro_export]
macro_rules! edit_field_date {
    ($instance:expr, $label:expr, $field:ident, $action:path) => {{
        let instance = $instance.clone();
        html! {
            <div class="tp__form-field tp__form-field__date">
                <$crate::app::components::date_input::DateInput
                    label={$label}
                    name={stringify!($field)}
                    value={instance.form.$field.clone()}
                    on_change={Callback::from(move |value: Option<i64>| {
                        instance.dispatch($action(value));
                    })}
                />
            </div>
        }
    }};
}

#[macro_export]
macro_rules! edit_field_list {
    ($instance:expr, $label:expr, $field:ident, $action:path, $placeholder:expr) => {{
        let instance = $instance.clone();
        let tag_list = instance.form.$field.iter()
              .map(|s| std::rc::Rc::new($crate::app::components::Tag { label: s.clone(), class: None }))
              .collect::<Vec<std::rc::Rc<$crate::app::components::Tag>>>();
        html! {
            <div class="tp__form-field tp__form-field__list">
                <label>{$label}</label>
                <$crate::app::components::TagList
                     tags={tag_list}
                     placeholder={$placeholder}
                     readonly={false}
                     on_change={Callback::from(move |value: Vec<std::rc::Rc<$crate::app::components::Tag>>| {
                        let list = value.iter().map(|t| t.label.clone()).collect();
                        instance.dispatch($action(list));
                     })}/>
            </div>
        }
    }};
}

#[macro_export]
macro_rules! edit_field_list_option {
    ($instance:expr, $label:expr, $field:ident, $action:path, $placeholder:expr) => {{
        let instance = $instance.clone();
        let tag_list = instance.form.$field.as_ref().map_or_else(Vec::new, |f| f.iter()
              .map(|s| std::rc::Rc::new($crate::app::components::Tag { label: s.clone(), class: None }))
              .collect::<Vec<std::rc::Rc<$crate::app::components::Tag>>>());
        html! {
            <div class="tp__form-field tp__form-field__list">
                <label>{$label}</label>
                <$crate::app::components::TagList
                     tags={tag_list}
                     placeholder={$placeholder}
                     readonly={false}
                     on_change={Callback::from(move |value: Vec< std::rc::Rc<$crate::app::components::Tag>>| {
                        let list: Vec<String> = value.iter().map(|t| t.label.clone()).collect();
                        if list.is_empty() {
                            instance.dispatch($action(None));
                        } else {
                            instance.dispatch($action(Some(list)));
                        }
                     })}/>
            </div>
        }
    }};
}

#[macro_export]
macro_rules! generate_form_reducer {
    (
        state: $state_name:ident { $data_field:ident: $data_type:ty },
        action_name: $action_name:ident,
        fields {
            $($set_name:ident => $field_name:ident : $field_type:ty),* $(,)?
        }
    ) => {
        #[derive(Debug, Clone, PartialEq)]
        pub struct $state_name {
            pub $data_field: $data_type,
            modified: bool,
        }

        impl $state_name {
            #[allow(dead_code)]
            pub fn modified(&self) -> bool {
                self.modified
            }
        }

        #[allow(clippy::large_enum_variant)]
        #[derive(Clone)]
        pub enum $action_name {
            $(
                $set_name($field_type),
            )*
            #[allow(dead_code)]
            SetAll($data_type),
        }

        impl yew::prelude::Reducible for $state_name {
            type Action = $action_name;

            fn reduce(self: std::rc::Rc<Self>, action: Self::Action) -> std::rc::Rc<Self> {
                let mut modified = self.modified;
                let new_data = match action {
                    $(
                        $action_name::$set_name(v) => {
                            let mut new_data = self.$data_field.clone();
                            new_data.$field_name = v.into();
                            if !modified { modified = true; }
                            new_data
                        },
                    )*
                    $action_name::SetAll(v) => {
                        modified = false;
                        v
                    },
                };
                $state_name { $data_field: new_data, modified }.into()
            }
        }

        impl $crate::app::components::config::HasFormData for $state_name {
            type Data = $data_type;

            fn data(&self) -> &Self::Data {
                &self.$data_field
            }

            // fn data_mut(&mut self) -> &mut Self::Data {
            //     &mut self.$data_field
            // }
        }
    };
}
