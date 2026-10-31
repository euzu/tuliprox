//! `#[tuliprox_plugin]`: the world export glue of one plugin module.

use crate::{
    hooks::{self, Hook, LIFECYCLE_INTERFACE, LIFECYCLE_TRAIT},
    manifest,
};
use proc_macro2::{Span, TokenStream};
use quote::{format_ident, quote};
use std::{collections::BTreeMap, fmt::Write as _};
use syn::{spanned::Spanned, Item, ItemMod, LitStr, Type};

/// The WIT package of the contract. `plugins/plugin-api` is a sibling directory of
/// this crate in the tuliprox repository.
const WIT_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../plugin-api/wit");

const SDK: &str = "::tuliprox_plugin_sdk";

/// What the module implements.
struct Found {
    lifecycle: Option<Type>,
    /// Interface -> (hook family, implementing type), in table order.
    hooks: BTreeMap<&'static str, (&'static Hook, Type, Span)>,
    settings: Option<syn::Ident>,
}

#[derive(Default)]
struct Args {
    manifest: Option<String>,
}

fn parse_args(attr: TokenStream) -> syn::Result<Args> {
    let mut args = Args::default();
    let parser = syn::meta::parser(|meta| {
        if meta.path.is_ident("manifest") {
            let lit: LitStr = meta.value()?.parse()?;
            args.manifest = Some(lit.value());
            Ok(())
        } else {
            Err(meta.error("unknown argument; the only one is `manifest = \"path/relative/to/the/crate\"`"))
        }
    });
    syn::parse::Parser::parse2(parser, attr)?;
    Ok(args)
}

fn last_segment(path: &syn::Path) -> String { path.segments.last().map(|s| s.ident.to_string()).unwrap_or_default() }

fn scan(items: &[Item]) -> syn::Result<Found> {
    let mut found = Found { lifecycle: None, hooks: BTreeMap::new(), settings: None };
    let mut errors: Option<syn::Error> = None;
    let mut push = |err: syn::Error| match &mut errors {
        Some(existing) => existing.combine(err),
        None => errors = Some(err),
    };
    for item in items {
        match item {
            Item::Impl(item_impl) => {
                let Some((_, path, _)) = &item_impl.trait_ else { continue };
                let name = last_segment(path);
                if name == LIFECYCLE_TRAIT {
                    if found.lifecycle.is_some() {
                        push(syn::Error::new_spanned(path, "`Lifecycle` is implemented twice in this module"));
                    } else {
                        found.lifecycle = Some((*item_impl.self_ty).clone());
                    }
                } else if let Some(hook) = hooks::hook_for_trait(&name) {
                    if found.hooks.contains_key(hook.interface) {
                        push(syn::Error::new_spanned(path, format!("`{name}` is implemented twice in this module")));
                    } else {
                        found.hooks.insert(hook.interface, (hook, (*item_impl.self_ty).clone(), path.span()));
                    }
                }
            }
            Item::Struct(item_struct)
                if item_struct.attrs.iter().any(|attr| last_segment(attr.path()) == "tuliprox_plugin_settings") =>
            {
                found.settings = Some(item_struct.ident.clone());
            }
            _ => {}
        }
    }
    errors.map_or(Ok(found), Err)
}

/// The manifest's hook families, or one error per problem.
fn check_against_manifest(
    found: &Found,
    manifest_hooks: &[String],
    manifest_path: &std::path::Path,
    module: &syn::Ident,
) -> syn::Result<()> {
    let mut errors: Option<syn::Error> = None;
    let mut push = |err: syn::Error| match &mut errors {
        Some(existing) => existing.combine(err),
        None => errors = Some(err),
    };
    let shown = manifest_path.display();

    let mut wanted: BTreeMap<&'static str, Vec<&str>> = BTreeMap::new();
    for id in manifest_hooks {
        match hooks::hook_for_id(id) {
            Some(hook) => wanted.entry(hook.interface).or_default().push(id),
            None => push(syn::Error::new(
                module.span(),
                format!("{shown} lists the unknown hook `{id}`; the ids are the `hooks:` table of the plugin plan"),
            )),
        }
    }
    for (interface, ids) in &wanted {
        if !found.hooks.contains_key(interface) {
            let hook = hooks::HOOKS.iter().find(|hook| hook.interface == *interface).expect("table row");
            push(syn::Error::new(
                module.span(),
                format!(
                    "{shown} lists `{}` but module `{module}` has no `impl {} for ...`; add the impl or remove the hook",
                    ids.join("`, `"),
                    hook.trait_name
                ),
            ));
        }
    }
    for (interface, (hook, _, span)) in &found.hooks {
        if !wanted.contains_key(interface) {
            push(syn::Error::new(
                *span,
                format!(
                    "`impl {}` exports `{}` but {shown} `hooks:` lists none of `{}`; the registry marks a plugin \
                     with an unlisted export invalid. Add one of them or remove the impl",
                    hook.trait_name,
                    hook.qualified(),
                    hook.ids.join("`, `")
                ),
            ));
        }
    }
    errors.map_or(Ok(()), Err)
}

/// One interface the component exports.
struct Export {
    alias: syn::Ident,
    ty: TokenStream,
    module: syn::Ident,
    macro_name: syn::Ident,
    interface: &'static str,
}

/// The type behind `lifecycle`: the author's, or a generated default that
/// parses the settings struct (if the module has one) from the init string.
fn lifecycle_type(found: &Found, items: &mut Vec<Item>) -> syn::Result<TokenStream> {
    let sdk = sdk_path();
    if let Some(ty) = &found.lifecycle {
        return Ok(quote!(#ty));
    }
    let init = found.settings.as_ref().map_or_else(
        || quote!(::core::result::Result::Ok(())),
        |name| quote!(<#name as #sdk::settings::PluginSettings>::init(settings)),
    );
    let default_ty = format_ident!("__TuliproxDefaultLifecycle");
    items.push(syn::parse2(quote! {
        #[doc(hidden)]
        pub struct #default_ty;
    })?);
    items.push(syn::parse2(quote! {
        impl #sdk::Lifecycle for #default_ty {
            fn init(settings: &str) -> ::core::result::Result<(), ::std::string::String> { #init }
        }
    })?);
    Ok(quote!(#default_ty))
}

fn sdk_path() -> TokenStream { SDK.parse().expect("path") }

/// Every export, `lifecycle` first.
fn exports_of(found: &Found, lifecycle_ty: TokenStream) -> Vec<Export> {
    let mut exports = vec![Export {
        alias: format_ident!("__TuliproxExportLifecycle"),
        ty: lifecycle_ty,
        module: format_ident!("lifecycle"),
        macro_name: format_ident!("__export_tuliprox_plugin_lifecycle_1_0_0_cabi"),
        interface: LIFECYCLE_INTERFACE,
    }];
    for (hook, ty, _) in found.hooks.values() {
        exports.push(Export {
            alias: format_ident!("__TuliproxExport{}", hook.trait_name),
            ty: quote!(#ty),
            module: format_ident!("{}", hook.module()),
            macro_name: format_ident!("{}", hook.export_macro()),
            interface: hook.interface,
        });
    }
    exports
}

/// The `generate!` call that declares the component's world.
///
/// The world is `base` (host imports + `lifecycle`) plus one `export` per
/// implemented interface. Everything it mentions is mapped with `with` onto the
/// SDK's own bindings, so the author's types and the SDK wrappers are one set;
/// only the component-type section (the world itself) is new.
fn world_module(exports: &[Export]) -> TokenStream {
    let sdk = sdk_path();
    let mut with: Vec<TokenStream> = Vec::new();
    for (key, path) in [
        ("tuliprox:plugin/types@1.0.0", quote!(#sdk::bindings::tuliprox::plugin::types)),
        ("tuliprox:plugin/host@1.0.0", quote!(#sdk::bindings::tuliprox::plugin::host)),
        ("wasi:io/poll@0.2.12", quote!(#sdk::bindings::wasi::io::poll)),
        ("wasi:io/error@0.2.12", quote!(#sdk::bindings::wasi::io::error)),
        ("wasi:io/streams@0.2.12", quote!(#sdk::bindings::wasi::io::streams)),
        ("wasi:clocks/monotonic-clock@0.2.12", quote!(#sdk::bindings::wasi::clocks::monotonic_clock)),
        ("wasi:http/types@0.2.12", quote!(#sdk::bindings::wasi::http::types)),
        ("wasi:http/outgoing-handler@0.2.12", quote!(#sdk::bindings::wasi::http::outgoing_handler)),
    ] {
        let key = LitStr::new(key, Span::call_site());
        with.push(quote!(#key: #path));
    }
    let mut type_only: Vec<&'static str> =
        exports.iter().flat_map(|export| hooks::type_interfaces(export.interface).iter().copied()).collect();
    type_only.sort_unstable();
    type_only.dedup();
    for name in type_only {
        let key = LitStr::new(&hooks::qualified(name), Span::call_site());
        let module = format_ident!("{}", name.replace('-', "_"));
        with.push(quote!(#key: #sdk::bindings::tuliprox::plugin::#module));
    }
    let mut world_exports = String::new();
    for export in exports {
        let key = LitStr::new(&hooks::qualified(export.interface), Span::call_site());
        let module = &export.module;
        with.push(quote!(#key: #sdk::bindings::exports::tuliprox::plugin::#module));
        if export.interface != LIFECYCLE_INTERFACE {
            let _ = writeln!(world_exports, "  export {};", hooks::qualified(export.interface));
        }
    }
    let wit = LitStr::new(
        &format!(
            "package tuliprox:guest;\nworld plugin {{\n  include tuliprox:plugin/base@1.0.0;\n{world_exports}}}\n"
        ),
        Span::call_site(),
    );
    let wit_dir = LitStr::new(WIT_DIR, Span::call_site());
    let runtime_path = LitStr::new("::tuliprox_plugin_sdk::__private::wit_bindgen::rt", Span::call_site());
    quote! {
        #[cfg(target_arch = "wasm32")]
        #[doc(hidden)]
        #[allow(clippy::all, clippy::pedantic, missing_docs, unused_qualifications, dead_code, unused_imports)]
        mod __tuliprox_world {
            #sdk::__private::wit_bindgen::generate!({
                path: #wit_dir,
                inline: #wit,
                world: "tuliprox:guest/plugin",
                runtime_path: #runtime_path,
                with: { #(#with),* },
            });
        }
        // Nothing else references the world's component-type section, and rustc
        // would drop it from the cdylib.
        #[cfg(target_arch = "wasm32")]
        #[used]
        static __TULIPROX_LINK_WORLD: fn() = __tuliprox_world::__link_custom_section_describing_imports;
    }
}

pub(crate) fn expand(attr: TokenStream, item: TokenStream) -> syn::Result<TokenStream> {
    let args = parse_args(attr)?;
    let mut module: ItemMod = syn::parse2(item).map_err(|err| {
        syn::Error::new(
            err.span(),
            "`#[tuliprox_plugin]` goes on an inline module that holds every hook impl: \
             `#[tuliprox_plugin] mod plugin { impl EventHooks for MyPlugin { .. } }`",
        )
    })?;
    let module_span = module.ident.span();
    let module_name = module.ident.clone();
    let Some((_, items)) = module.content.as_mut() else {
        return Err(syn::Error::new_spanned(
            &module,
            "`#[tuliprox_plugin]` needs an inline module (`mod plugin { .. }`): the macro must see every impl",
        ));
    };

    let found = scan(items)?;
    let manifest_path = manifest::locate(args.manifest.as_deref()).map_err(|msg| syn::Error::new(module_span, msg))?;
    let manifest_hooks = manifest::read_hooks(&manifest_path).map_err(|msg| syn::Error::new(module_span, msg))?;
    check_against_manifest(&found, &manifest_hooks, &manifest_path, &module_name)?;

    let sdk = sdk_path();
    let lifecycle_ty = lifecycle_type(&found, items)?;
    let exports = exports_of(&found, lifecycle_ty);
    let manifest_lit = manifest_path.to_string_lossy().into_owned();
    let world = world_module(&exports);
    let exports_mod = quote!(#sdk::bindings::exports::tuliprox::plugin);

    let aliases = exports.iter().map(|export| {
        let Export { alias, ty, module, .. } = export;
        quote! {
            #[doc(hidden)]
            type #alias = #sdk::__private::Adapter<#ty>;
            // The glue type-checks on every target, not just wasm32.
            const _: fn() = || {
                fn exported<T: #exports_mod::#module::Guest>() {}
                exported::<#alias>();
            };
        }
    });
    let invocations = exports.iter().map(|export| {
        let Export { alias, module, macro_name, .. } = export;
        quote! {
            #[cfg(target_arch = "wasm32")]
            #exports_mod::#module::#macro_name!(#alias with_types_in #exports_mod::#module);
        }
    });
    let manifest_ids = manifest_hooks.iter();
    let export_names = exports.iter().map(|export| hooks::qualified(export.interface));

    let generated = quote! {
        // Makes cargo rebuild the plugin when the manifest changes.
        const _: &[u8] = ::core::include_bytes!(#manifest_lit);

        /// The manifest `hooks:` ids this plugin was built against.
        #[allow(dead_code)]
        pub const HOOK_IDS: &[&str] = &[#(#manifest_ids),*];

        /// The component exports, `lifecycle` first.
        #[allow(dead_code)]
        pub const EXPORTS: &[&str] = &[#(#export_names),*];

        #(#aliases)*
        #world
        #(#invocations)*
    };
    items.extend(syn::parse2::<syn::File>(generated)?.items);
    Ok(quote!(#module))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn module(src: &str) -> ItemMod { syn::parse_str(src).expect("module") }

    fn check(src: &str, hooks: &[&str]) -> Result<(), String> {
        let module = module(src);
        let items = &module.content.as_ref().expect("inline").1;
        let found = scan(items).map_err(|err| err.to_string())?;
        let hooks: Vec<String> = hooks.iter().map(|id| (*id).to_string()).collect();
        check_against_manifest(&found, &hooks, Path::new("manifest.yml"), &module.ident)
            .map_err(|err| err.into_iter().map(|e| e.to_string()).collect::<Vec<_>>().join(" | "))
    }

    const TWO: &str = "mod plugin { impl PlaylistHooks for P {} impl tuliprox_plugin_sdk::EventHooks for P {} }";

    #[test]
    fn a_matching_manifest_passes_and_per_stage_ids_share_one_interface() {
        assert_eq!(check(TWO, &["playlist.after-filter", "playlist.before-map", "events.on-event"]), Ok(()));
    }

    #[test]
    fn a_hook_without_an_impl_is_named() {
        let err = check(TWO, &["playlist.after-filter", "events.on-event", "metrics.collect"]).unwrap_err();
        assert!(err.contains("lists `metrics.collect`") && err.contains("impl MetricsHooks"), "{err}");
    }

    #[test]
    fn an_impl_without_a_hook_is_named() {
        let err = check(TWO, &["playlist.after-filter"]).unwrap_err();
        assert!(err.contains("impl EventHooks") && err.contains("tuliprox:plugin/event-hooks@1.0.0"), "{err}");
    }

    #[test]
    fn an_unknown_id_and_a_duplicate_impl_are_errors() {
        let err = check(TWO, &["playlist.after-filter", "events.on-event", "playlist.nope"]).unwrap_err();
        assert!(err.contains("unknown hook `playlist.nope`"), "{err}");
        let twice = "mod plugin { impl Tasks for A {} impl Tasks for B {} }";
        assert!(check(twice, &["tasks.run"]).unwrap_err().contains("implemented twice"));
    }

    #[test]
    fn lifecycle_and_settings_are_detected_without_a_manifest_hook() {
        let src = "mod plugin { #[tuliprox_plugin_settings] struct S { a: u8 } impl Lifecycle for P {} }";
        let module = module(src);
        let found = scan(&module.content.as_ref().expect("inline").1).unwrap();
        assert!(found.lifecycle.is_some() && found.hooks.is_empty());
        assert_eq!(found.settings.map(|ident| ident.to_string()).as_deref(), Some("S"));
    }
}
