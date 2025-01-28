mod args;
mod tunnel;

use std::{collections::HashSet, sync::Arc};

use anyhow::Result;
use args::Args;
use clap::Parser;
use tracing::info;
use tunnel::{Config, Server};

#[tokio::main]
pub async fn main() -> Result<()> {
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .compact()
        .finish();
    tracing::subscriber::set_global_default(subscriber).unwrap();

    let args = Args::parse();
    let config = Config {
        domain: "todo.com".to_owned(),
        require_auth: false,
        auth_tokens: HashSet::new(),
        port: args.port,
        supported_protocols: args.protocols,
        cert_path: args.cert_path,
        key_path: args.key_path,
    };
    let server = Arc::new(Server::new(config));

    info!("Starting server...");
    server.start().await
}
