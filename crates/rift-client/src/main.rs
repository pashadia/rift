//! Rift Client Binary

use anyhow::Result;
use clap::Parser;

#[derive(Parser)]
#[command(name = "rift-client")]
#[command(about = "Rift network filesystem client")]
struct Args {
    /// Client configuration file
    #[arg(short, long, default_value = "~/.config/rift/client.toml")]
    config: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let args = Args::parse();
    tracing::info!("Starting Rift client with config: {}", args.config);

    // Placeholder - actual implementation in Phase 5
    println!("Rift client (placeholder)");
    
    Ok(())
}
