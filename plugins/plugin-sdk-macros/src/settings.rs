//! `#[tuliprox_plugin_settings]`: a typed settings struct.

use proc_macro2::TokenStream;
use quote::quote;
use syn::ItemStruct;

pub(crate) fn expand(attr: TokenStream, item: TokenStream) -> syn::Result<TokenStream> {
    if !attr.is_empty() {
        return Err(syn::Error::new_spanned(attr, "`#[tuliprox_plugin_settings]` takes no arguments"));
    }
    let item: ItemStruct = syn::parse2(item).map_err(|err| {
        syn::Error::new(err.span(), "`#[tuliprox_plugin_settings]` goes on a struct with named fields")
    })?;
    if !item.generics.params.is_empty() {
        return Err(syn::Error::new_spanned(&item.generics, "a settings struct cannot be generic"));
    }
    let name = &item.ident;
    Ok(quote! {
        #[derive(::tuliprox_plugin_sdk::__private::serde::Deserialize)]
        #[serde(crate = "::tuliprox_plugin_sdk::__private::serde")]
        #item

        impl ::tuliprox_plugin_sdk::settings::PluginSettings for #name {
            fn slot() -> &'static ::tuliprox_plugin_sdk::settings::SettingsSlot<Self> {
                static SLOT: ::tuliprox_plugin_sdk::settings::SettingsSlot<#name> =
                    ::tuliprox_plugin_sdk::settings::SettingsSlot::new();
                &SLOT
            }
        }
    })
}
