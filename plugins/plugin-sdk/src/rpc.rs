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

use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::{collections::BTreeMap, fmt::Display};

/// An option entry returned by an options RPC export (`x-options-rpc`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RpcOption {
    pub value: String,
    pub label: String,
}

impl RpcOption {
    #[must_use]
    pub fn new(value: impl Into<String>, label: impl Into<String>) -> Self {
        Self { value: value.into(), label: label.into() }
    }
}

/// The input context handed to an `input_scoped` RPC export.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RpcInputContext {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub password: Option<String>,
    #[serde(default)]
    pub options: BTreeMap<String, serde_json::Value>,
}

/// The envelope received by an `input_scoped` RPC export.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScopedRequest<A = serde_json::Value> {
    pub args: A,
    pub input: RpcInputContext,
}

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

    /// Registers `handler` for an `input_scoped` RPC export: the incoming payload
    /// `{ "args": ..., "input": ... }` is parsed into [`ScopedRequest<A>`].
    #[must_use]
    pub fn route_scoped<A, O, E>(
        self,
        method: &'static str,
        handler: impl Fn(ScopedRequest<A>) -> Result<O, E> + 'static,
    ) -> Self
    where
        A: DeserializeOwned,
        O: Serialize,
        E: Display,
    {
        self.route(method, handler)
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

    #[test]
    fn scoped_routes_receive_args_and_input_context() {
        let r = Router::new().route_scoped("get-options", |req: ScopedRequest<serde_json::Value>| {
            let filter = req.args["filter"].as_str().unwrap_or_default();
            let opt = RpcOption::new("main", format!("{}:{filter}", req.input.name));
            Ok::<_, String>(vec![opt])
        });
        let payload = r#"{"args":{"filter":"active"},"input":{"name":"my-server","url":"http://srv"}}"#;
        let res = r.dispatch("get-options", payload).unwrap();
        assert_eq!(res, r#"[{"value":"main","label":"my-server:active"}]"#);
    }
}
