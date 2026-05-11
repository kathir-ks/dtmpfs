use std::sync::atomic::Ordering;
use std::sync::Arc;

use crate::state::StoreState;

pub fn spawn_debug_http(state: Arc<StoreState>, bind: String) {
    use axum::{routing::get, Json, Router};

    let app = Router::new().route(
        "/debug/blocks",
        get(move || {
            let state = state.clone();
            async move {
                let mut blocks = Vec::new();
                for entry in state.blocks.iter() {
                    let (k, v) = (entry.key(), entry.value());
                    blocks.push(serde_json::json!({
                        "ino":  k.ino.0,
                        "idx":  k.block_idx.0,
                        "gen":  k.generation.0,
                        "len":  v.len(),
                    }));
                }
                Json(serde_json::json!({
                    "count":    blocks.len(),
                    "ram_used": state.ram_used.load(Ordering::Relaxed),
                    "blocks":   blocks,
                }))
            }
        }),
    );

    tokio::spawn(async move {
        let listener = tokio::net::TcpListener::bind(&bind).await.unwrap();
        axum::serve(listener, app).await.unwrap();
    });
}
