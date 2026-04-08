//! Rift Server Binary

use std::net::SocketAddr;
use std::path::PathBuf;

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
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let args = Args::parse();

    if !args.share.exists() {
        anyhow::bail!("share path does not exist: {}", args.share.display());
    }

    // TODO(v1): load a persistent cert from /etc/rift/server.{cert,key}.
    let cert = rcgen::generate_simple_self_signed(vec!["rift-server".to_string()])?;
    let cert_der = cert.cert.der().to_vec();
    let key_der = cert.key_pair.serialize_der();

    let fingerprint = rift_transport::cert_fingerprint(&cert_der);
    tracing::info!(addr = %args.addr, share = %args.share.display(), "starting rift-server");
    println!("Server fingerprint : {fingerprint}");
    println!("Listening on       : {}", args.addr);
    println!("Exporting          : {}", args.share.display());

    let listener = rift_transport::server_endpoint(args.addr, &cert_der, &key_der)?;

    tokio::select! {
        result = rift_server::server::accept_loop(listener, args.share) => result,
        _ = tokio::signal::ctrl_c() => {
            println!("\nShutting down.");
            Ok(())
        }
    }
}
