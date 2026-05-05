//! Rift Server Binary

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use clap::Parser;
use rift_common::crypto::Chunker;
use rift_server::metadata::db::Database;
use rift_server::server::RequestContext;

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

    /// Path to the Merkle cache database.
    #[arg(long)]
    db_path: Option<PathBuf>,
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
            config.db_path = args.db_path.clone();
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
    let (cert_der, key_der) = rift_server::cert::get_or_create_cert(
        server_config.cert_path.as_ref(),
        server_config.key_path.as_ref(),
    )?;

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

    let db_path = server_config.db_path.clone().unwrap_or_else(|| {
        dirs::data_local_dir()
            .unwrap_or_else(|| PathBuf::from("/tmp"))
            .join("rift")
            .join("merkle_cache.db")
    });
    let db: Arc<rift_server::metadata::db::Database> =
        Arc::new(rift_server::metadata::db::Database::open(&db_path).await?);
    let handle_db = Arc::new(rift_server::handle::HandleDatabase::new());

    let share_path = server_config.shares[0].path.clone();

    let ctx = RequestContext {
        share: share_path.clone(),
        db: db.clone(),
        handle_db,
        chunker: server_config.chunker,
    };

    spawn_background_check(db, share_path, server_config.chunker);

    tokio::select! {
        result = rift_server::server::accept_loop(listener, ctx) => result,
        _ = tokio::signal::ctrl_c() => {
            println!("\nShutting down.");
            Ok(())
        }
    }
}

/// Spawn the background cache integrity check as a separate tokio task.
///
/// The check walks the share once on startup, recomputing any missing or
/// stale Merkle tree entries and cleaning up orphaned DB rows.
fn spawn_background_check(db: Arc<Database>, share: PathBuf, chunker: Chunker) {
    tokio::spawn(async move {
        tracing::info!(share = %share.display(), "starting background cache integrity check");
        match rift_server::background_check::run_background_check(&share, db, chunker).await {
            Ok(summary) => {
                tracing::info!(
                    files_checked = summary.files_checked,
                    files_added = summary.files_added,
                    files_conflict = summary.files_conflict,
                    files_cleaned = summary.files_cleaned,
                    errors = summary.errors,
                    "background check complete"
                );
            }
            Err(e) => {
                tracing::error!(error = %e, "background cache integrity check failed");
            }
        }
    });
}
