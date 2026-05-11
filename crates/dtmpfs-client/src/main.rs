mod cache;
mod client;
mod flush;
mod fs;
mod open_file;

use fs::DtmpfsFs;

#[derive(clap::Parser)]
struct Cli {
    #[arg(long)]
    config: String,
}

fn main() -> anyhow::Result<()> {
    use clap::Parser;
    let cli = Cli::parse();
    tracing_subscriber::fmt::init();

    let cfg = dtmpfs_common::config::load(&cli.config)?;
    let dtmpfs_common::config::Config::Client(client_cfg) = cfg else {
        anyhow::bail!("expected role=client in config");
    };

    let mut rt_builder = tokio::runtime::Builder::new_multi_thread();
    rt_builder.enable_all();
    if let Some(n) = client_cfg.tokio_worker_threads {
        if n > 0 { rt_builder.worker_threads(n as usize); }
    }
    let rt = rt_builder.build()?;
    let handle = rt.handle().clone();
    let fs = rt.block_on(build_fs(handle, &client_cfg))?;

    let mut opts = vec![
        fuser::MountOption::FSName("dtmpfs".into()),
        fuser::MountOption::Subtype("dtmpfs".into()),
    ];
    if client_cfg.mount_options.default_permissions {
        opts.push(fuser::MountOption::DefaultPermissions);
    }
    if client_cfg.mount_options.allow_other {
        opts.push(fuser::MountOption::AllowOther);
    }
    if client_cfg.mount_options.auto_unmount {
        opts.push(fuser::MountOption::AutoUnmount);
    }
    if client_cfg.mount_options.no_atime {
        opts.push(fuser::MountOption::NoAtime);
    }

    fuser::mount2(fs, &client_cfg.mount_point, &opts)?;
    Ok(())
}

async fn build_fs(
    handle: tokio::runtime::Handle,
    cfg: &dtmpfs_common::config::ClientConfig,
) -> anyhow::Result<DtmpfsFs> {
    use dtmpfs_proto::meta::meta_client::MetaClient;
    use tonic::transport::Channel;
    use cache::{build_attr_cache, build_block_cache};
    use client::StoreClientPool;

    let meta_chan = Channel::from_shared(cfg.meta_addr.clone())?.connect().await?;
    let meta      = tokio::sync::Mutex::new(MetaClient::new(meta_chan));
    let stores    = StoreClientPool::new();
    let attr_cache  = build_attr_cache(cfg.attr_cache_ttl_ms);
    let block_cache = build_block_cache(cfg.block_cache_capacity_mb);

    Ok(DtmpfsFs {
        rt: handle,
        meta,
        stores,
        attr_cache,
        block_cache,
        open_files: dashmap::DashMap::new(),
        block_size: cfg.block_size,
        replication_factor: cfg.replication_factor,
        token: cfg.cluster_token.clone(),
    })
}
