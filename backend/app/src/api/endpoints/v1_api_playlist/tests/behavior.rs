use axum::{extract::Query, response::IntoResponse, Json, Router};
use serde_json::json;
use std::collections::HashMap;

pub(in crate::api::endpoints::v1_api_playlist::tests) async fn stalker_mock_handler(
    Query(params): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    let action = params.get("action").map_or("", String::as_str);
    let portal_type = params.get("type").map_or("", String::as_str);
    let page = params.get("p").map_or("1", String::as_str);
    let response = match (portal_type, action, page) {
        ("stb", "handshake", _) => json!({"js": {"token": "preview-token"}}),
        ("stb", "get_profile", _) => json!({"js": {"status": 1, "max_connections": 1}}),
        ("stb", "get_capabilities", _) => json!({"js": {}}),
        ("itv", "get_genres", _) => json!({"js": [{"id": "10", "title": "News"}]}),
        ("itv", "get_ordered_list", "1") => json!({
            "js": {
                "data": {
                    "101": {
                        "id": "101",
                        "name": "Demo Channel",
                        "category_id": "10",
                        "cmd": "ffmpeg http://streams.example/live/101"
                    },
                    "102": {
                        "id": "102",
                        "name": "Private Channel",
                        "category_id": "10",
                        "cmd": "ffmpeg http://streams.example/live/102"
                    }
                }
            }
        }),
        ("itv", "create_link", _) => {
            let destination = if params.get("cmd").is_some_and(|cmd| cmd.ends_with("/102")) {
                "http://127.0.0.1/live/102"
            } else {
                "http://8.8.8.8/live/101"
            };
            json!({"js": {"cmd": format!("ffmpeg {destination}")}})
        }
        _ => json!({"js": []}),
    };
    Json(response)
}

pub(in crate::api::endpoints::v1_api_playlist::tests) async fn spawn_stalker_mock_server(
) -> (String, tokio::task::JoinHandle<()>) {
    let router = Router::new().route("/server/load.php", axum::routing::get(stalker_mock_handler));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind mock stalker server");
    let base_url = format!("http://{}", listener.local_addr().expect("mock addr"));
    let handle = tokio::spawn(async move {
        axum::serve(listener, router).await.expect("serve mock stalker server");
    });
    (base_url, handle)
}
