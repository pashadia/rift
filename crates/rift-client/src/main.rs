//! Rift Client Binary

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "rift-client")]
#[command(about = "Rift network filesystem client")]
struct Args {
    /// Client configuration file
    #[arg(short = 'c', long, default_value = "~/.config/rift/client.toml")]
    config: String,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Mount a Rift filesystem at the given path (Linux only)
    Mount {
        /// Directory to mount the filesystem at
        path: std::path::PathBuf,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let args = Args::parse();
    tracing::info!(config = %args.config, "Starting Rift client");

    match args.command {
        Command::Mount { path } => {
            #[cfg(not(target_os = "linux"))]
            {
                let _ = path;
                anyhow::bail!("mount is only supported on Linux");
            }

            #[cfg(target_os = "linux")]
            {
                tracing::info!(mountpoint = %path.display(), "Mounting Rift filesystem");
                let _session = rift_client::mount::mount(&path)?;
                tracing::info!(mountpoint = %path.display(), "Filesystem mounted, waiting for Ctrl+C");
                println!("Mounted at {}, press Ctrl+C to unmount", path.display());
                tokio::signal::ctrl_c().await?;
                tracing::info!(mountpoint = %path.display(), "Received signal, unmounting");
                println!("Unmounting...");
                // _session dropped here → AutoUnmount
            }
        }
    }

    Ok(())
}
