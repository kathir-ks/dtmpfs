use std::collections::HashMap;
use std::sync::Arc;
use arc_swap::ArcSwap;
use dashmap::DashMap;
use tonic::transport::Channel;

use dtmpfs_proto::store::store_client::StoreClient;
use dtmpfs_common::error::DtmpfsError;
use dtmpfs_common::id::NodeId;

pub struct StoreClientPool {
    pub clients: DashMap<NodeId, StoreClient<Channel>>,
    pub addrs:   ArcSwap<HashMap<NodeId, String>>,
}

impl StoreClientPool {
    pub fn new() -> Arc<Self> {
        Arc::new(StoreClientPool {
            clients: DashMap::new(),
            addrs:   ArcSwap::from_pointee(HashMap::new()),
        })
    }

    pub async fn get(&self, id: &NodeId) -> Result<StoreClient<Channel>, DtmpfsError> {
        if let Some(c) = self.clients.get(id) { return Ok(c.clone()); }
        let addrs = self.addrs.load();
        let addr  = addrs.get(id).ok_or_else(|| DtmpfsError::StoreUnavailable(id.clone()))?;
        let chan  = Channel::from_shared(addr.clone())
            .map_err(|_| DtmpfsError::StoreUnavailable(id.clone()))?
            .connect().await
            .map_err(|_| DtmpfsError::StoreUnavailable(id.clone()))?;
        let client = StoreClient::new(chan);
        self.clients.insert(id.clone(), client.clone());
        Ok(client)
    }

    pub fn refresh_addrs(&self, m: HashMap<NodeId, String>) {
        self.addrs.store(Arc::new(m));
    }
}
