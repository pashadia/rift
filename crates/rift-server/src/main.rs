//! Rift Server Binary

use anyhow::Result;
use clap::Parser;

#[derive(Parser)]
#[command(name = "rift-server")]
#[command(about = "Rift network filesystem server")]
struct Args {
    /// Server configuration file
    #[arg(short, long, default_value = "/etc/rift/server.toml")]
    config: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let args = Args::parse();
    tracing::info!("Starting Rift server with config: {}", args.config);

    // Placeholder - actual implementation in Phase 4
    println!("Rift server (placeholder)");
    
    Ok(())
}
