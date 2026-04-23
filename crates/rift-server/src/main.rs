//! Rift Server Binary

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use clap::Parser;

#[derive(Parser)]
#[command(name = "rift-server")]
#[command(about = "Rift network filesystem server")]
struct Args {
    /// Directory to export as a Rift share.
    #[arg(long)]
    share: Option<PathBuf>,

    /// Address to listen on.
    #[arg(long)]
    addr: Option<SocketAddr>,

    /// TLS certificate file. If not specified, uses ~/.config/rift/server.cert
    /// or generates a new one on first boot.
    #[arg(long)]
    cert: Option<PathBuf>,

    /// TLS private key file. Required if --cert is specified.
    #[arg(long)]
    key: Option<PathBuf>,

    /// Path to TOML configuration file.
    #[arg(long)]
    config: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<()> {
    use tracing_subscriber::prelude::*;

    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn"));

    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer().with_target(true))
        .with(filter)
        .init();

    let args = Args::parse();

    let server_config = match &args.config {
        Some(path) => rift_server::config::load_config(path)?,
        None => {
            let mut config = rift_common::config::ServerConfig::default();
            if let Some(addr) = args.addr {
                config.listen_addr = addr.to_string();
            }
            if let Some(share) = &args.share {
                if !share.exists() {
                    anyhow::bail!("share path does not exist: {}", share.display());
                }
                config.shares.push(rift_common::config::ShareConfig {
                    name: "default".to_string(),
                    path: share.clone(),
                    read_only: false,
                    permissions: Default::default(),
                });
            }
            config.cert_path = args.cert.clone();
            config.key_path = args.key.clone();
            config
        }
    };

    if server_config.shares.is_empty() {
        anyhow::bail!("no shares configured; use --share or --config to define at least one share");
    }

    for share in &server_config.shares {
        if !share.path.exists() {
            anyhow::bail!("share path does not exist: {}", share.path.display());
        }
    }

    let listen_addr: SocketAddr = server_config.listen_addr.parse()?;
    let (cert_der, key_der) =
        rift_server::cert::get_or_create_cert(server_config.cert_path, server_config.key_path)?;

    let fingerprint = rift_transport::cert_fingerprint(&cert_der);
    tracing::info!(addr = %listen_addr, "starting rift-server");
    println!("Server fingerprint : {fingerprint}");
    println!("Listening on       : {listen_addr}");

    for share in &server_config.shares {
        println!(
            "Exporting          : {} ({})",
            share.name,
            share.path.display()
        );
    }

    let listener = rift_transport::server_endpoint(listen_addr, &cert_der, &key_der)?;

    let db: Arc<Option<rift_server::metadata::db::Database>> = Arc::new(None);
    let handle_db = Arc::new(rift_server::handle::HandleDatabase::new());

    let share_path = server_config.shares[0].path.clone();

    tokio::select! {
        result = rift_server::server::accept_loop(listener, share_path, db, handle_db, server_config.chunker) => result,
        _ = tokio::signal::ctrl_c() => {
            println!("\nShutting down.");
            Ok(())
        }
    }
}
