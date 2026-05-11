use std::sync::Arc;
use tokio::sync::RwLock;
use crate::state::MetaState;

pub fn spawn_debug_http(state: Arc<RwLock<MetaState>>, bind: String) {
    use axum::{routing::get, Json, Router};
    let app = Router::new().route("/debug/state", get(move || {
        let state = state.clone();
        async move {
            let s = state.read().await;
            Json(serde_json::json!({
                "inodes":       s.inodes.len(),
                "open_handles": s.open_handles.len(),
                "nodes":        s.nodes.keys().map(|n| n.as_str()).collect::<Vec<_>>(),
            }))
        }
    }));
    tokio::spawn(async move {
        let listener = tokio::net::TcpListener::bind(&bind).await.unwrap();
        axum::serve(listener, app).await.unwrap();
    });
}
