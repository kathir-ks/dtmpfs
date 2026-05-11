use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use dtmpfs_proto::meta::{meta_client::MetaClient, HeartbeatReq};

use crate::state::StoreState;

pub fn spawn_heartbeat(state: Arc<StoreState>, advertise_addr: String, token: String) {
    let meta_addr = state.meta_addr.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(1));
        let epoch_s = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let mut client = loop {
            match MetaClient::connect(meta_addr.clone()).await {
                Ok(c) => break c,
                Err(e) => {
                    tracing::warn!(?e, "heartbeat: waiting for meta");
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
            }
        };
        loop {
            interval.tick().await;
            let req = HeartbeatReq {
                node_id:        state.node_id.0.clone(),
                addr:           advertise_addr.clone(),
                used_bytes:     state.ram_used.load(Ordering::Relaxed),
                capacity_bytes: state.ram_budget,
                epoch_s,
            };
            let mut rpc = tonic::Request::new(req);
            rpc.metadata_mut().insert(
                "cluster-token",
                token.parse().expect("token is valid ascii"),
            );
            if let Err(e) = client.heartbeat_node(rpc).await {
                tracing::warn!(?e, "heartbeat failed");
            }
        }
    });
}
