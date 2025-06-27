use yew::prelude::*;
use crate::hooks::use_icon_context;

#[derive(Properties, Clone, PartialEq, Eq, Debug)]
pub struct SvgIconProps {
    name: AttrValue,
    path: AttrValue,
    #[prop_or(AttrValue::Static("24px"))]
    pub width: AttrValue,
    #[prop_or(AttrValue::Static("24px"))]
    pub height: AttrValue,
    #[prop_or(AttrValue::Static("0 0 24 24"))]
    pub viewport:AttrValue,
}
#[function_component]
pub fn SvgIcon(props: &SvgIconProps) -> Html {
    // info!("SvgIcon props {:?}", props);
    html! {
      <svg class={format!("svg-icon icon-{}", props.name.clone())} fill="inherit" focusable="false" aria-hidden="true" data-testid={props.name.clone()} height={format!("{}", props.height.clone())} width={format!("{}", props.width.clone())} viewBox={props.viewport.clone()}>
        <path d={props.path.clone()}/>
      </svg>
   }
}

#[derive(Properties, Clone, PartialEq, Eq)]
pub struct AppIconProps {
    pub name: AttrValue,
    #[prop_or(AttrValue::Static("100%"))]
    pub width: AttrValue,
    #[prop_or(AttrValue::Static("100%"))]
    pub height: AttrValue,
}

#[function_component]
pub fn AppIcon(props: &AppIconProps) -> Html {
    let icon_ctx = use_icon_context();
    let name = props.name.to_string();
    let icon_def = use_memo((icon_ctx, name.clone()), |(icon_ctx, name)| {
        icon_ctx.get_icon(name)
    });

    match &*icon_def {
        Some(definition) => {
            let viewport = definition.viewport.as_ref().map_or_else(|| AttrValue::Static("0 0 24 24"), |vp|AttrValue::from(vp.to_string()));
            html! {
                <SvgIcon path={(*definition.path).to_string()} name={name} width={props.width.clone()} height={props.height.clone()} viewport={viewport}/>
            }
        },
        None => html! {}
    }
}
