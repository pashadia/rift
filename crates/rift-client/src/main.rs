//! Rift Client Binary

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
        path: std::path::PathBuf,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let args = Args::parse();

    match args.command {
        Command::Mount { server, share, path } => {
            #[cfg(not(target_os = "linux"))]
            {
                let _ = (server, share, path);
                anyhow::bail!("mount is only supported on Linux");
            }

            #[cfg(target_os = "linux")]
            {
                let addr: std::net::SocketAddr = server.parse()?;
                tracing::info!(server = %addr, share = %share, mountpoint = %path.display(), "Connecting");

                // TODO(rift-client): replace with real RiftClient::connect once implemented
                let _ = (addr, share);
                todo!("wire up RiftClient::connect — implement rift-client first");

                // The code below is the intended final form; it will compile once
                // RiftClient exists:
                //
                // let client = rift_client::client::RiftClient::connect(addr, &share).await?;
                // let root_handle = client.root_handle().to_vec();
                // let rt = tokio::runtime::Handle::current();
                // let _session = rift_client::mount::mount(
                //     Box::new(client),
                //     root_handle,
                //     rt,
                //     &path,
                // )?;
                // println!("Mounted {} at {}, press Ctrl+C to unmount", share, path.display());
                // tokio::signal::ctrl_c().await?;
            }
        }
    }

    #[allow(unreachable_code)]
    Ok(())
}
