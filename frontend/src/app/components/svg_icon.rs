use crate::hooks::use_icon_context;
use yew::prelude::*;

#[derive(Properties, Clone, PartialEq, Eq, Debug)]
pub struct SvgIconProps {
    name: AttrValue,
    path: AttrValue,
    #[prop_or(AttrValue::Static("24px"))]
    pub width: AttrValue,
    #[prop_or(AttrValue::Static("24px"))]
    pub height: AttrValue,
    #[prop_or(AttrValue::Static("0 0 24 24"))]
    pub viewport: AttrValue,
}
#[component]
pub fn SvgIcon(props: &SvgIconProps) -> Html {
    // info!("SvgIcon props {:?}", props);
    html! {
       <svg class={format!("svg-icon icon-{}", props.name.clone())} fill="inherit" focusable="false" aria-hidden="true" data-testid={props.name.clone()} height={format!("{}", props.height.clone())} width={format!("{}", props.width.clone())} viewBox={props.viewport.clone()}>
         <path d={props.path.clone()}/>
       </svg>
    }
}

#[derive(Properties, Clone, PartialEq)]
pub struct AppIconProps {
    pub name: AttrValue,
    #[prop_or(AttrValue::Static("100%"))]
    pub width: AttrValue,
    #[prop_or(AttrValue::Static("100%"))]
    pub height: AttrValue,
}

#[component]
pub fn AppIcon(props: &AppIconProps) -> Html {
    let icon_ctx = use_icon_context();
    let name = props.name.to_string();
    let icon_def = use_memo((icon_ctx, name.clone()), |(icon_ctx, name)| icon_ctx.get_icon(name));

    match &*icon_def {
        Some(definition) => {
            let viewport = definition
                .viewport
                .as_ref()
                .map_or_else(|| AttrValue::Static("0 0 24 24"), |vp| AttrValue::from(vp.clone()));
            html! {
                <SvgIcon path={(*definition.path).to_string()} name={name} width={props.width.clone()} height={props.height.clone()} viewport={viewport}/>
            }
        }
        None => html! { <img src={props.name.to_string()} /> },
    }
}

#[cfg(test)]
mod icon_catalog_tests {
    use std::collections::BTreeSet;

    /// Every icon name the catalog in `public/assets/icons.json` provides.
    fn catalog() -> BTreeSet<String> {
        let entries: serde_json::Value =
            serde_json::from_str(include_str!("../../../public/assets/icons.json")).expect("icons.json parses");
        let mut known = BTreeSet::new();
        for entry in entries.as_array().into_iter().flatten() {
            for key in entry.get("keys").and_then(serde_json::Value::as_array).into_iter().flatten() {
                if let Some(name) = key.as_str() {
                    known.insert(name.to_string());
                }
            }
        }
        assert!(!known.is_empty(), "icons.json yielded no icon names");
        known
    }

    fn collect_rs_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                collect_rs_files(&path, out);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                out.push(path);
            }
        }
    }

    /// `AppIcon` falls back to `<img src="{name}">` for a name the catalog does not know, so a typo renders a
    /// broken image instead of failing the build. This walks the sources for literal `name="..."` arguments and
    /// refuses any name the catalog cannot resolve. Dynamic `name={...}` expressions are not covered: their value
    /// is only known at runtime.
    #[test]
    fn every_literal_app_icon_name_exists_in_the_icon_catalog() {
        let known = catalog();

        // Assembled from parts so this test's own source does not match the pattern it searches for.
        let attribute = "name=";
        let needle = format!("AppIcon {attribute}\"");

        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        collect_rs_files(&root, &mut files);
        assert!(!files.is_empty(), "no Rust sources found under {}", root.display());

        let mut missing = Vec::new();
        for file in files {
            let Ok(text) = std::fs::read_to_string(&file) else { continue };
            for (start, _) in text.match_indices(&needle) {
                let rest = &text[start + needle.len()..];
                let Some(end) = rest.find('"') else { continue };
                let name = &rest[..end];
                if !known.contains(name) {
                    missing.push(format!("{} -> AppIcon name=\"{name}\"", file.display()));
                }
            }
        }

        assert!(missing.is_empty(), "icon names absent from icons.json:\n{}", missing.join("\n"));
    }
}
