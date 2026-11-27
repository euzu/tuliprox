use axum::{
    extract::{Query, State},
    response::IntoResponse,
    Json, Router,
};
use serde_json::json;
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

pub(in crate::api) type StalkerCreateLinkHandler = Arc<dyn Fn(usize, &HashMap<String, String>) -> String + Send + Sync>;

#[derive(Clone, Default)]
pub(in crate::api) struct StalkerMockConfig {
    pub(in crate::api) create_link: Option<StalkerCreateLinkHandler>,
}

#[derive(Clone)]
struct StalkerMockState {
    create_link: Option<StalkerCreateLinkHandler>,
    create_link_calls: Arc<AtomicUsize>,
}

pub(in crate::api) struct StalkerMockServer {
    pub(in crate::api) base_url: String,
    create_link_calls: Arc<AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
}

impl StalkerMockServer {
    pub(in crate::api) fn create_link_call_count(&self) -> usize { self.create_link_calls.load(Ordering::SeqCst) }
}

impl Drop for StalkerMockServer {
    fn drop(&mut self) { self.task.abort(); }
}

fn default_create_link_destination(params: &HashMap<String, String>) -> String {
    if params.get("cmd").is_some_and(|cmd| cmd.ends_with("/102")) {
        "http://127.0.0.1/live/102".to_string()
    } else {
        "http://8.8.8.8/live/101".to_string()
    }
}

async fn stalker_mock_handler_with_state(
    State(state): State<StalkerMockState>,
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
            let call_index = state.create_link_calls.fetch_add(1, Ordering::SeqCst) + 1;
            let destination = match state.create_link.as_ref() {
                Some(handler) => handler(call_index, &params),
                None => default_create_link_destination(&params),
            };
            json!({"js": {"cmd": format!("ffmpeg {destination}")}})
        }
        _ => json!({"js": []}),
    };
    Json(response)
}

async fn spawn_stalker_mock_server_inner(
    config: StalkerMockConfig,
) -> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
    let create_link_calls = Arc::new(AtomicUsize::new(0));
    let state = StalkerMockState { create_link: config.create_link, create_link_calls: Arc::clone(&create_link_calls) };
    let router =
        Router::new().route("/server/load.php", axum::routing::get(stalker_mock_handler_with_state)).with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind mock stalker server");
    let base_url = format!("http://{}", listener.local_addr().expect("mock addr"));
    let handle = tokio::spawn(async move {
        axum::serve(listener, router).await.expect("serve mock stalker server");
    });
    (base_url, create_link_calls, handle)
}

pub(in crate::api) async fn spawn_stalker_mock_server() -> (String, tokio::task::JoinHandle<()>) {
    let (base_url, _, handle) = spawn_stalker_mock_server_inner(StalkerMockConfig::default()).await;
    (base_url, handle)
}

pub(in crate::api) async fn spawn_stalker_mock_server_with(config: StalkerMockConfig) -> StalkerMockServer {
    let (base_url, create_link_calls, task) = spawn_stalker_mock_server_inner(config).await;
    StalkerMockServer { base_url, create_link_calls, task }
}
