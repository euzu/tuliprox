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

    /// Extracts the string literals that can become an icon name from one `name=` argument.
    ///
    /// Only literals that are valid icon identifiers are returned: a literal such as `"icon-{}"` belongs to a
    /// `format!` call and is not a catalog name, so the character filter keeps pattern text out of the check.
    fn literal_names(argument: &str) -> Vec<String> {
        let mut names = Vec::new();
        let mut rest = argument;
        while let Some(open) = rest.find('"') {
            let after_open = &rest[open + 1..];
            let Some(close) = after_open.find('"') else { break };
            let literal = &after_open[..close];
            if !literal.is_empty() && literal.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                names.push(literal.to_string());
            }
            rest = &after_open[close + 1..];
        }
        names
    }

    /// Returns the `name=` argument of the `AppIcon` element that starts at `start`.
    ///
    /// Both forms are handled: the shorthand `name="Icon"` and the braced expression `name={ ... }`, which may carry
    /// one literal or several (an `if`/`else` between two icon names). The expression is brace-matched rather than
    /// searched for the closing quote, because it can span lines and nest.
    fn name_argument(text: &str, start: usize) -> Option<&str> {
        let tag = &text[start..];
        let attribute = tag.find("name=")?;
        let value = &tag[attribute + "name=".len()..];
        if let Some(quoted) = value.strip_prefix('"') {
            let close = quoted.find('"')?;
            return Some(&value[..close + 2]);
        }
        let braced = value.strip_prefix('{')?;
        let mut depth = 1usize;
        for (index, ch) in braced.char_indices() {
            match ch {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(&braced[..index]);
                    }
                }
                _ => {}
            }
        }
        None
    }

    /// `AppIcon` falls back to `<img src="{name}">` for a name the catalog does not know, so a typo renders a
    /// broken image instead of failing the build. This walks the sources for every `AppIcon` element and refuses any
    /// literal icon name the catalog cannot resolve, in both the `name="Icon"` and the `name={ ... }` form.
    /// Names that are computed at runtime (a field, a function call without a literal) cannot be checked here.
    #[test]
    fn every_literal_app_icon_name_exists_in_the_icon_catalog() {
        let known = catalog();

        // Assembled from parts so this test's own source does not match the pattern it searches for.
        let element = "AppIcon";
        let mut needle = String::from("<");
        needle.push_str(element);

        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        collect_rs_files(&root, &mut files);
        assert!(!files.is_empty(), "no Rust sources found under {}", root.display());

        let mut missing = Vec::new();
        let mut checked = 0usize;
        for file in files {
            let Ok(text) = std::fs::read_to_string(&file) else { continue };
            for (start, _) in text.match_indices(&needle) {
                let Some(argument) = name_argument(&text, start) else { continue };
                for name in literal_names(argument) {
                    checked += 1;
                    if !known.contains(&name) {
                        missing.push(format!("{} -> AppIcon name={name}", file.display()));
                    }
                }
            }
        }

        assert!(checked > 0, "no literal AppIcon names found; the scan is broken, not the catalog");
        assert!(missing.is_empty(), "icon names absent from icons.json:\n{}", missing.join("\n"));
    }
}
