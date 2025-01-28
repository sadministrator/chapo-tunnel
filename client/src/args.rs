use common::protocol;

use std::path::PathBuf;

use clap::Parser;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
pub struct Args {
    /// Directory to share
    #[arg(short, long)]
    pub share: PathBuf,

    /// Local file server port
    #[arg(short, long, default_value = "3001")]
    pub file_server_port: u16,

    /// Tunnel server address
    #[arg(long)]
    pub remote_host: String,

    /// Tunnel server port
    #[arg(long)]
    pub remote_port: u16,

    /// Comma-separated list of protocols to request
    #[arg(
        long,
        value_delimiter = ',',
        value_parser,
        default_value = "http,tcp,udp"
    )]
    pub protocols: Vec<protocol::ProtocolType>,

    /// Authentication token
    pub auth_token: Option<String>,
}
