//! `PluginSystem::api_rpc_call`: the refusals that need a loaded plugin
//! (plan §18 *W6b seam freeze* "rpc"). The limits and the status of the payload
//! checks (`403`, `413`, `429`, `400`, `422`) are exercised without a component
//! by the unit tests in `src/rpc.rs`; a call that reaches a guest needs a
//! component exporting `rpc`, which the `ui-demo` fixture provides.

mod common;
mod ui_support;

use http::StatusCode;
use tuliprox_plugins::PluginSystem;
use ui_support::{stage, ID};

async fn call(system: &PluginSystem, id: &str, method: &str) -> StatusCode {
    system
        .api_rpc_call(id, method, b"{}", || panic!("a refused call must not ask for the write permission"))
        .await
        .map_or_else(|failure| failure.status, |_| StatusCode::OK)
}

#[tokio::test]
async fn an_unknown_plugin_is_a_404() {
    let Some(staged) = stage(Some(true)) else { return };
    assert_eq!(call(&staged.system, "nobody", "ping").await, StatusCode::NOT_FOUND);
    assert_eq!(call(&staged.system, "../probe-bare", "ping").await, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_plugin_that_is_not_enabled_is_a_409() {
    let Some(disabled) = stage(Some(false)) else { return };
    assert_eq!(call(&disabled.system, ID, "ping").await, StatusCode::CONFLICT);
    let Some(unlisted) = stage(None) else { return };
    assert_eq!(call(&unlisted.system, ID, "ping").await, StatusCode::CONFLICT);
}

#[tokio::test]
async fn a_method_the_manifest_does_not_export_is_a_404() {
    let Some(staged) = stage(Some(true)) else { return };
    // The staged manifest declares no `ui.rpc_exports` at all.
    for method in ["ping", "", "../ping", "get-fixtures"] {
        assert_eq!(call(&staged.system, ID, method).await, StatusCode::NOT_FOUND, "{method:?}");
    }
}
