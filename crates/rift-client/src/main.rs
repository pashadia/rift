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
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let args = Args::parse();

    match args.command {
        Command::Mount {
            server,
            share,
            path,
        } => {
            #[cfg(not(target_os = "linux"))]
            {
                let _ = (server, share, path);
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
                    "connecting to server"
                );

                let client = rift_client::client::RiftClient::connect(addr, &share).await?;
                let view = rift_client::view::RiftShareView::new(std::sync::Arc::new(client));

                println!(
                    "Connected — server fingerprint: {}",
                    "todo" // This needs to be exposed on the view or client
                );

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
