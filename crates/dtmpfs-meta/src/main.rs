mod auth;
mod debug;
mod service;
mod state;

use auth::check_token;
use debug::spawn_debug_http;
use service::MetaService;
use state::{MetaState, spawn_heartbeat_watcher};
use std::time::Duration;
use tonic::transport::Server;
use dtmpfs_proto::meta::meta_server::MetaServer;

#[derive(clap::Parser)]
#[command(name = "metasrv", about = "dtmpfs metadata server")]
struct Cli {
    #[arg(long)]
    config: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    use clap::Parser;
    let cli = Cli::parse();
    tracing_subscriber::fmt::init();
    let cfg = dtmpfs_common::config::load(&cli.config)?;
    let dtmpfs_common::config::Config::Meta(meta_cfg) = cfg else {
        anyhow::bail!("expected role=meta in config");
    };
    let state = MetaState::new(meta_cfg.replication_factor);
    let dead = Duration::from_millis(meta_cfg.heartbeat_timeout_ms);
    spawn_heartbeat_watcher(state.clone(), dead);
    if let Some(bind) = meta_cfg.debug_http_listen.clone() {
        spawn_debug_http(state.clone(), bind);
    }
    let token = meta_cfg.cluster_token.clone();
    let svc = MetaService { state, token: token.clone() };
    let addr = meta_cfg.listen.parse()?;
    Server::builder()
        .add_service(MetaServer::with_interceptor(svc, move |req| {
            check_token(req, &token)
        }))
        .serve(addr)
        .await?;
    Ok(())
}
