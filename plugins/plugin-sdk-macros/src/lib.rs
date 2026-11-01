//! Proc macros of `tuliprox-plugin-sdk`. Use them through the SDK, which
//! re-exports both.

mod hooks;
mod manifest;
mod plugin;
mod settings;

use proc_macro::TokenStream;

/// Turns an inline module of hook impls into a plugin component.
///
/// ```ignore
/// use tuliprox_plugin_sdk::prelude::*;
///
/// #[tuliprox_plugin]
/// mod plugin {
///     use super::*;
///     struct Enricher;
///     impl PlaylistHooks for Enricher {
///         fn on_stage(stage: Stage, target: &str, channels: Vec<Channel>) -> Result<Vec<ChannelChange>, String> {
///             Ok(Vec::new())
///         }
///     }
/// }
/// ```
///
/// The attribute goes on an inline `mod` rather than on each `impl` because the
/// component's export set is one decision: the macro must see every impl to
/// export `lifecycle` plus exactly the hook interfaces implemented (an extra
/// export makes the registry mark the plugin invalid), to supply a default
/// `lifecycle` when none is written, and to check the set against the
/// crate's `manifest.yml` `hooks:` in one place.
///
/// Recognised impls (matched by the last path segment): `Lifecycle` and the
/// hook traits of the SDK (`PlaylistHooks`, `EventHooks`, `Tasks`, ...). The
/// manifest is read from `$CARGO_MANIFEST_DIR/manifest.yml`, or from
/// `#[tuliprox_plugin(manifest = "path")]` relative to the crate. Any mismatch
/// between its `hooks:` and the impls is a compile error. The module also gets
/// the consts `HOOK_IDS` (the manifest ids) and `EXPORTS` (the component
/// exports).
///
/// Declare the `#[tuliprox_plugin_settings]` struct inside the module: when no
/// `impl Lifecycle` is written, the generated `lifecycle.init` parses it from
/// the settings JSON, so a bad configuration disables the instance with the
/// parse error. (With your own `impl Lifecycle`, call `Settings::init(settings)?`.)
#[proc_macro_attribute]
pub fn tuliprox_plugin(attr: TokenStream, item: TokenStream) -> TokenStream {
    plugin::expand(attr.into(), item.into()).unwrap_or_else(syn::Error::into_compile_error).into()
}

/// Declares a typed settings struct (deserialized from the `init` string or
/// `get-config()`). Fields of type `Secret<String>` never print their value.
///
/// Adds `serde::Deserialize` and implements `PluginSettings`, which provides
/// `init`, `current` and `reload`. Unknown settings keys are ignored.
#[proc_macro_attribute]
pub fn tuliprox_plugin_settings(attr: TokenStream, item: TokenStream) -> TokenStream {
    settings::expand(attr.into(), item.into()).unwrap_or_else(syn::Error::into_compile_error).into()
}
