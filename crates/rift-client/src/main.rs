//! Rift Client Binary

use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "rift-client")]
#[command(about = "Rift network filesystem client")]
struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Mount a Rift share at the given path (Linux only)
    Mount {
        /// Rift server address, e.g. 127.0.0.1:4433
        #[arg(long)]
        server: String,

        /// Share name to mount
        #[arg(long, default_value = "demo")]
        share: String,

        /// Local directory to mount the filesystem at
        path: PathBuf,

        /// Directory for local cache (optional)
        #[arg(long)]
        cache_dir: Option<PathBuf>,
    },
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

    match args.command {
        Command::Mount {
            server,
            share,
            path,
            cache_dir,
        } => {
            #[cfg(not(target_os = "linux"))]
            {
                let _ = (server, share, path, cache_dir);
                anyhow::bail!("mount is only supported on Linux");
            }

            #[cfg(all(target_os = "linux", feature = "fuse"))]
            {
                use fuse3::path::Session;
                use fuse3::MountOptions;
                use rift_client::fuse::RiftFilesystem;

                let addr: SocketAddr = server
                    .parse()
                    .map_err(|_| anyhow::anyhow!("invalid server address: {server}"))?;

                tracing::info!(
                    server = %addr,
                    share  = %share,
                    mountpoint = %path.display(),
                    cache_dir = ?cache_dir,
                    "connecting to server"
                );

                let client = rift_client::client::RiftClient::connect(addr, &share).await?;
                let fingerprint = client.server_fingerprint().to_string();

                let view = if let Some(dir) = cache_dir {
                    rift_client::view::RiftShareView::with_cache(std::sync::Arc::new(client), dir)
                        .await?
                } else {
                    rift_client::view::RiftShareView::new(std::sync::Arc::new(client))
                };

                println!("Connected — server fingerprint: {fingerprint}");

                let mut options = MountOptions::default();
                options.fs_name("rift");

                let fs = RiftFilesystem::new(std::sync::Arc::new(view));
                let mut mount_handle = Session::new(options)
                    .mount_with_unprivileged(fs, &path)
                    .await?;

                let handle = &mut mount_handle;

                println!("Mounted '{}' at {}", share, path.display());
                println!("Press Ctrl-C to unmount.");
                tokio::select! {
                    r = handle => { r.map_err(|e| anyhow::anyhow!("{e}"))? }
                    _ = tokio::signal::ctrl_c() => {
                        mount_handle.unmount().await?;
                    }
                }
                println!("\nUnmounting.");
            }
        }
    }

    Ok(())
}
