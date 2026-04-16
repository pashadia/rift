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
    share: PathBuf,

    /// Address to listen on.
    #[arg(long, default_value = "0.0.0.0:4433")]
    addr: SocketAddr,

    /// TLS certificate file. If not specified, uses ~/.config/rift/server.cert
    /// or generates a new one on first boot.
    #[arg(long)]
    cert: Option<PathBuf>,

    /// TLS private key file. Required if --cert is specified.
    #[arg(long)]
    key: Option<PathBuf>,
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

    if !args.share.exists() {
        anyhow::bail!("share path does not exist: {}", args.share.display());
    }

    let (cert_der, key_der) = rift_server::cert::get_or_create_cert(args.cert, args.key)?;

    let fingerprint = rift_transport::cert_fingerprint(&cert_der);
    tracing::info!(addr = %args.addr, share = %args.share.display(), "starting rift-server");
    println!("Server fingerprint : {fingerprint}");
    println!("Listening on       : {}", args.addr);
    println!("Exporting          : {}", args.share.display());

    let listener = rift_transport::server_endpoint(args.addr, &cert_der, &key_der)?;

    let db: Arc<Option<rift_server::metadata::db::Database>> = Arc::new(None);
    let handle_db = Arc::new(rift_server::handle::HandleDatabase::new());

    tokio::select! {
        result = rift_server::server::accept_loop(listener, args.share, db, handle_db) => result,
        _ = tokio::signal::ctrl_c() => {
            println!("\nShutting down.");
            Ok(())
        }
    }
}
