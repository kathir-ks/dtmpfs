mod auth;
mod debug;
mod heartbeat;
mod service;
mod state;

use auth::check_token;
use debug::spawn_debug_http;
use heartbeat::spawn_heartbeat;
use service::StoreService;
use state::StoreState;
use tonic::transport::Server;
use dtmpfs_proto::store::store_server::StoreServer;

#[derive(clap::Parser)]
#[command(name = "storesrv", about = "dtmpfs store server")]
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
    let dtmpfs_common::config::Config::Store(store_cfg) = cfg else {
        anyhow::bail!("expected role=store in config");
    };
    let state = StoreState::new(
        store_cfg.node_id.clone(),
        store_cfg.meta_addr.clone(),
        store_cfg.ram_budget_bytes,
    );
    spawn_heartbeat(state.clone(), store_cfg.advertise_addr.clone(), store_cfg.cluster_token.clone());
    if let Some(bind) = store_cfg.debug_http_listen.clone() {
        spawn_debug_http(state.clone(), bind);
    }
    let token = store_cfg.cluster_token.clone();
    let svc = StoreService { state };
    let addr = store_cfg.listen.parse()?;
    Server::builder()
        .add_service(StoreServer::with_interceptor(svc, move |req| {
            check_token(req, &token)
        }))
        .serve(addr)
        .await?;
    Ok(())
}
