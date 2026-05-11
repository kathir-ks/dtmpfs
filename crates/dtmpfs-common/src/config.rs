use serde::{Deserialize, Serialize};
use crate::id::NodeId;

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "role", rename_all = "lowercase")]
pub enum Config {
    Meta(MetaConfig),
    Store(StoreConfig),
    Client(ClientConfig),
}

#[derive(Debug, Deserialize, Serialize)]
pub struct MetaConfig {
    pub node_id:              NodeId,
    pub listen:               String,
    pub cluster_token:        String,
    #[serde(default = "d_replication")]
    pub replication_factor:   usize,
    #[serde(default = "d_block_size")]
    pub block_size:           usize,
    #[serde(default = "d_heartbeat_timeout_ms")]
    pub heartbeat_timeout_ms: u64,
    #[serde(default = "d_max_open_handles")]
    pub max_open_handles:     u64,
    #[serde(default)]
    pub gc_interval_ms:       Option<u64>,
    #[serde(default)]
    pub debug_http_listen:    Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct StoreConfig {
    pub node_id:           NodeId,
    pub listen:            String,
    pub advertise_addr:    String,
    pub meta_addr:         String,
    pub cluster_token:     String,
    pub ram_budget_bytes:  u64,
    #[serde(default)]
    pub debug_http_listen: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct ClientConfig {
    pub node_id:                 NodeId,
    pub mount_point:             String,
    pub meta_addr:               String,
    pub cluster_token:           String,
    #[serde(default = "d_block_size")]
    pub block_size:              usize,
    #[serde(default = "d_replication")]
    pub replication_factor:      usize,
    #[serde(default = "d_attr_ttl_ms")]
    pub attr_cache_ttl_ms:       u64,
    #[serde(default = "d_block_cache_mb")]
    pub block_cache_capacity_mb: u64,
    #[serde(default = "d_fuse_threads")]
    pub fuse_threads:            usize,
    #[serde(default)]
    pub tokio_worker_threads:    Option<u32>,
    #[serde(default = "d_keepalive_secs")]
    pub keepalive_interval_secs: u64,
    #[serde(default = "d_rpc_timeout_ms")]
    pub rpc_timeout_ms:          u64,
    #[serde(default = "d_write_rpc_timeout_ms")]
    pub write_rpc_timeout_ms:    u64,
    #[serde(default)]
    pub mount_options:           ClientMountOptions,
}

#[derive(Debug, Deserialize, Serialize, Default)]
pub struct ClientMountOptions {
    #[serde(default = "d_true")] pub allow_other:         bool,
    #[serde(default = "d_true")] pub default_permissions: bool,
    #[serde(default = "d_true")] pub auto_unmount:        bool,
    #[serde(default = "d_true")] pub no_atime:            bool,
}

fn d_replication() -> usize { 1 }
fn d_block_size() -> usize { 1 << 20 }
fn d_heartbeat_timeout_ms() -> u64 { 5000 }
fn d_attr_ttl_ms() -> u64 { 1000 }
fn d_block_cache_mb() -> u64 { 1024 }
fn d_fuse_threads() -> usize { 4 }
fn d_max_open_handles() -> u64 { 100_000 }
fn d_keepalive_secs() -> u64 { 30 }
fn d_rpc_timeout_ms() -> u64 { 5000 }
fn d_write_rpc_timeout_ms() -> u64 { 30_000 }
fn d_true() -> bool { true }

pub fn load(path: &str) -> anyhow::Result<Config> {
    let s = std::fs::read_to_string(path)?;
    Ok(toml::from_str(&s)?)
}
