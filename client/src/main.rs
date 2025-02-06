mod args;
mod file_server;
mod tunnel;
mod utils;

use anyhow::Result;
use args::Args;
use clap::Parser;
use file_server::FileServer;
use tracing_subscriber::{fmt, EnvFilter};

#[tokio::main]
async fn main() -> Result<()> {
    let filter = EnvFilter::new("debug").add_directive("hyper=info".parse().unwrap());

    let subscriber = fmt().with_env_filter(filter).compact().finish();
    tracing::subscriber::set_global_default(subscriber).unwrap();

    let args = Args::parse();
    let version = "0.1.0".to_owned(); // todo: grab value from Cargo.toml
    let config = tunnel::Config::new(
        args.file_server_port,
        args.remote_host,
        args.remote_port,
        args.protocols,
        version,
        args.auth_token,
    );

    if !args.share.exists() {
        anyhow::bail!("Shared directory does not exist");
    }

    let file_server = FileServer::new(args.share, args.file_server_port);
    let file_server_handle = tokio::spawn(file_server.run());

    let client = tunnel::Client::new(config);
    let client_handle = tokio::spawn(client.run());

    let _ = tokio::try_join!(file_server_handle, client_handle)?;

    Ok(())
}
