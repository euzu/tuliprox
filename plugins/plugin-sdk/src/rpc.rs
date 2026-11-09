//! RPC dispatch for the `rpc` hook: one `call` export, many manifest
//! `ui.rpc_exports`, told apart by `method`. Payload and result are JSON; the
//! host has already validated the payload against the declared `input_schema`.
//!
//! ```ignore
//! impl Rpc for Plugin {
//!     fn call(method: &str, payload: &str) -> Result<String, String> {
//!         rpc::Router::new()
//!             .route("lookup-team", lookup_team)   // fn(LookupRequest) -> Result<Team, String>
//!             .route("clear-cache", clear_cache)   // fn(()) -> Result<(), String>
//!             .dispatch(method, payload)
//!     }
//! }
//! ```

use serde::{de::DeserializeOwned, Serialize};
use std::fmt::Display;

type Handler = Box<dyn Fn(&str) -> Result<String, String>>;

/// Routes an RPC method name to a typed handler.
#[derive(Default)]
pub struct Router {
    routes: Vec<(&'static str, Handler)>,
}

impl Router {
    #[must_use]
    pub fn new() -> Self { Self::default() }

    /// Registers `handler` for `method`: the payload is parsed as `I`, the
    /// result serialized as JSON. A handler error is its `Display`. An empty
    /// payload is read as `null`, so `()` and `Option<_>` inputs work.
    #[must_use]
    pub fn route<I, O, E>(mut self, method: &'static str, handler: impl Fn(I) -> Result<O, E> + 'static) -> Self
    where
        I: DeserializeOwned,
        O: Serialize,
        E: Display,
    {
        self.routes.push((
            method,
            Box::new(move |payload| {
                let payload = if payload.trim().is_empty() { "null" } else { payload };
                let input: I =
                    serde_json::from_str(payload).map_err(|err| format!("invalid `{method}` payload: {err}"))?;
                let output = handler(input).map_err(|err| err.to_string())?;
                serde_json::to_string(&output).map_err(|err| format!("`{method}` result: {err}"))
            }),
        ));
        self
    }

    /// Calls the handler registered for `method`; an unknown method is an
    /// error naming the registered ones.
    pub fn dispatch(&self, method: &str, payload: &str) -> Result<String, String> {
        match self.routes.iter().find(|(name, _)| *name == method) {
            Some((_, handler)) => handler(payload),
            None => Err(format!(
                "unknown rpc method `{method}`; this plugin serves: {}",
                self.routes.iter().map(|(name, _)| *name).collect::<Vec<_>>().join(", ")
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Deserialize)]
    struct Lookup {
        name: String,
    }

    fn router() -> Router {
        Router::new()
            .route("lookup", |req: Lookup| Ok::<_, String>(serde_json::json!({ "found": req.name })))
            .route("ping", |(): ()| Ok::<_, String>("pong"))
            .route("fail", |_: serde_json::Value| Err::<(), _>("no luck"))
    }

    #[test]
    fn methods_dispatch_with_typed_payloads() {
        assert_eq!(router().dispatch("lookup", r#"{"name":"ann"}"#).unwrap(), r#"{"found":"ann"}"#);
        assert_eq!(router().dispatch("ping", "").unwrap(), r#""pong""#);
        assert_eq!(router().dispatch("fail", "{}"), Err("no luck".to_string()));
    }

    #[test]
    fn bad_payloads_and_unknown_methods_are_errors() {
        let bad = router().dispatch("lookup", r#"{"nom":1}"#).unwrap_err();
        assert!(bad.starts_with("invalid `lookup` payload:"));
        let unknown = router().dispatch("zap", "").unwrap_err();
        assert!(unknown.contains("`zap`") && unknown.contains("lookup, ping, fail"));
    }
}
