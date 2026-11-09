use tuliprox_plugin_sdk::prelude::*;
#[tuliprox_plugin]
mod plugin {
    use super::*;
    pub struct Probe;
    impl HttpRoutes for Probe {
        fn authenticate(req: &HttpRouteRequest) -> Result<Option<String>, String> {
            if req.query == "auth-error" {
                return Err("private-auth-secret".into());
            }
            if req.route == "login" {
                return Ok(Some("anonymous-login".into()));
            }
            Ok(req.headers.iter().any(|(k, v)| k == "x-api-key" && v == "fixture-key").then(|| "external-user".into()))
        }
        fn handle(req: HttpRouteRequest) -> Result<HttpRouteResponse, String> {
            if req.query == "trap" {
                panic!("private-handler-secret");
            }
            let count: u64 = kv::get_json("calls")?.unwrap_or(0);
            kv::set_json("calls", &(count + 1))?;
            let body = serde_json::json!({"route":req.route, "method":req.method, "path":req.path,
                "params":req.params, "query":req.query, "principal":req.principal, "body":req.body,
                "headers":req.headers, "calls": count+1})
            .to_string()
            .into_bytes();
            Ok(HttpRouteResponse {
                status: if req.query == "bad-status" { 101 } else { 201 },
                headers: vec![
                    ("content-type".into(), "application/json".into()),
                    ("connection".into(), "x-hop".into()),
                    ("x-hop".into(), "drop".into()),
                    ("set-cookie".into(), "host-session=evil".into()),
                ],
                body,
            })
        }
    }
}
