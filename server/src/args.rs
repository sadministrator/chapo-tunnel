use common::protocol;

use clap::Parser;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
pub struct Args {
    /// Port server uses to listen for upstream clients
    #[arg(short, long)]
    pub listen: u16,

    /// Path to TLS certificate
    #[arg(short, long)]
    pub cert_path: String,

    /// Path to key associated with certificate
    #[arg(short, long)]
    pub key_path: String,

    /// Comma-separated list of supported protocols
    #[arg(long, value_delimiter = ',', value_parser)]
    pub protocols: Vec<protocol::ProtocolType>,
}
