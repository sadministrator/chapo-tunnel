use std::{collections::HashSet, sync::Arc};

use anyhow::{anyhow, Result};
use common::protocol::{
    self, Data, DownstreamClient, HttpData, Message, ProtocolType, Tunnel, UpstreamClient,
};
use dashmap::DashMap;
use rustls_pemfile::{self, certs};
use tokio::{
    fs::File,
    io::{AsyncReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
};
use tokio_rustls::{
    rustls::{pki_types::PrivateKeyDer, ServerConfig},
    TlsAcceptor,
};
use tracing::{debug, error, info, trace, warn};

use crate::utils::{self, TlsWriter};

pub struct Config {
    pub domain: String,
    pub upstream_listen: u16,

    pub require_auth: bool,
    pub auth_tokens: HashSet<String>,

    pub supported_protocols: Vec<ProtocolType>,

    pub cert_path: String,
    pub key_path: String,
}

pub struct Server {
    // subdomain -> upstream client
    pub upstream_clients: DashMap<String, UpstreamClient>,

    // stream ID -> tunnel
    pub tunnels: DashMap<u32, Tunnel>,

    pub config: Config,
}

impl Server {
    pub fn new(config: Config) -> Self {
        let upstream_clients = DashMap::new();
        let tunnels = DashMap::new();

        Self {
            upstream_clients,
            tunnels,
            config,
        }
    }

    pub async fn run(self: Arc<Self>) -> Result<()> {
        let upstream = self.clone();
        let downstream = self.clone();

        let upstream_handle = tokio::spawn(async move { upstream.listen_upstream().await });
        let downstream_handle =
            tokio::spawn(async move { downstream.listen_downstream_http().await });

        let _ = tokio::join!(upstream_handle, downstream_handle);

        Ok(())
    }

    async fn listen_upstream(&self) -> Result<()> {
        let listener =
            TcpListener::bind(format!("0.0.0.0:{}", self.config.upstream_listen)).await?;
        info!(
            "Listening for upstream clients at port {}",
            self.config.upstream_listen
        );

        while let Ok((mut stream, addr)) = listener.accept().await {
            info!("New upstream connection initiated by {addr}");

            let handshake = protocol::read_message(&mut stream).await?;
            debug!("Received handshake: {:?}", handshake);

            match handshake {
                Message::HandshakeRequest {
                    supported_version,
                    supported_protocols,
                    auth_token,
                } => {
                    let accepted_protocols: Vec<ProtocolType> = supported_protocols
                        .into_iter()
                        .filter(|p| self.config.supported_protocols.contains(p))
                        .collect();
                    let subdomain = self.generate_subdomain()?;
                    let response = Message::HandshakeResponse {
                        accepted_protocols: accepted_protocols.clone(),
                        accepted_version: supported_version,
                        subdomain: subdomain.clone(),
                    };

                    protocol::write_message(&mut stream, &response).await?;
                    debug!("Handshake response sent");

                    let connection = UpstreamClient::new(stream, accepted_protocols);

                    self.upstream_clients.insert(subdomain.clone(), connection);
                    info!("{addr} added to upstream connections with subdomain \"{subdomain}\"");
                }
                _ => error!("Unable to connect to {addr}, handshake not received"),
            }
        }

        Ok(())
    }

    async fn listen_downstream_http(&self) -> Result<()> {
        let tls_config = Arc::new(self.create_tls_config().await?);
        let acceptor = TlsAcceptor::from(tls_config);
        let listener = TcpListener::bind("0.0.0.0:4433").await?;
        info!("Listening downstream for HTTPS");

        while let Ok((stream, addr)) = listener.accept().await {
            info!("New HTTPS connection from: {addr}");

            let acceptor = acceptor.clone();
            let upstream_clients = self.upstream_clients.clone();
            let tunnels = self.tunnels.clone();

            tokio::spawn(async move {
                if let Err(e) =
                    Self::handle_connection(stream, acceptor, upstream_clients, tunnels).await
                {
                    error!("HTTPS connection error from {addr}: {e}");
                }
            });
        }

        Ok(())
    }

    async fn handle_connection(
        downstream: TcpStream,
        acceptor: TlsAcceptor,
        upstream_clients: DashMap<String, UpstreamClient>,
        tunnels: DashMap<u32, Tunnel>,
    ) -> Result<()> {
        const STREAM_THRESHOLD: usize = 1024;

        let tls_stream = acceptor.accept(downstream).await?;
        let tls_reader = BufReader::new(tls_stream);
        let downstream_client = DownstreamClient::new(tls_reader);

        loop {
            let mut downguard = downstream_client.stream.lock().await;
            let mut buf = vec![0u8; 1024];
            let n = downguard.read(&mut buf).await?;
            let request = utils::parse_http_request(&buf[..n])?;
            debug!("Received from downstream: {:?}", request);

            let HttpData::Request { headers, .. } = request.clone() else {
                return Err(anyhow!("Expected HTTP request"));
            };

            let subdomain = utils::extract_subdomain(&headers)?;
            let upstream_client = upstream_clients
                .get(&subdomain)
                .ok_or(anyhow!("No upstream found for subdomain \"{subdomain}\""))?;
            let mut upstream = upstream_client.stream.clone();

            let is_chunked = headers
                .get("transfer-encoding")
                .map_or(false, |v| v.eq_ignore_ascii_case("chunked"));
            let content_len = headers
                .get("content-length")
                .and_then(|v| v.parse::<usize>().ok());

            if !is_chunked || content_len.is_some_and(|len| len < STREAM_THRESHOLD) {
                let message = Message::Data(Data::Http(request));

                protocol::write_message_locked(&upstream, &message).await?;
                debug!("Forwarded request upstream: {:?}", message);
            } else {
                let tunnel = Tunnel {
                    upstream: upstream_client.clone(),
                    downstream: downstream_client.clone(),
                };
                let stream_open = Message::stream_open(ProtocolType::Http);
                let Message::StreamOpen { stream_id, .. } = stream_open else {
                    return Err(anyhow!("Expected stream open"));
                };
                tunnels.insert(stream_id, tunnel);

                protocol::write_message_locked(&mut upstream, &stream_open).await?;

                loop {
                    let mut buf = [0u8; 1024];
                    let n = downguard.read(&mut buf).await?;
                    let is_end = n == 0;
                    let chunk = Message::Data(Data::Http(HttpData::BodyChunk {
                        stream_id,
                        data: buf[..n].to_vec(),
                        is_end,
                    }));

                    protocol::write_message_locked(&mut upstream, &chunk).await?;

                    if is_end {
                        break;
                    }
                }
            }

            let response = protocol::read_message_locked(&mut upstream).await?;
            debug!("Received from upstream: {:?}", response);

            match response {
                Message::StreamOpen {
                    stream_id,
                    protocol,
                } => {
                    debug!("Upstream at {subdomain} opened stream {stream_id}");
                    let request_header = protocol::read_message_locked(&mut upstream).await?;
                    let Message::Data(Data::Http(HttpData::Response {
                        status, headers, ..
                    })) = request_header
                    else {
                        return Err(anyhow!("Expected HTTP request header"));
                    };
                    let response_string = utils::build_response_string(status, &headers)?;

                    downguard.write_all(response_string.as_bytes()).await?;
                    debug!("Sent response header to downstream: {:?}", response_string);

                    let mut downwriter = TlsWriter::new(&mut downguard);
                    loop {
                        let stream_chunk = protocol::read_message_locked(&mut upstream).await?;
                        trace!("{:?}", stream_chunk);

                        match stream_chunk {
                            Message::Data(Data::Http(HttpData::BodyChunk {
                                stream_id,
                                data,
                                is_end,
                            })) => {
                                downwriter.write_chunk(&data).await?;

                                if is_end {
                                    downwriter.write_final_chunk().await?;
                                }
                            }
                            Message::StreamClose { stream_id } => {
                                debug!("Upstream at {subdomain} closed stream {stream_id}");
                                tunnels.remove(&stream_id);
                                break;
                            }
                            _ => {
                                warn!(
                                    "Received unexpected message from upstream: {:?}",
                                    stream_chunk
                                );
                                break;
                            }
                        }
                    }
                }
                Message::Data(Data::Http(http_data)) => match http_data {
                    HttpData::Response {
                        status,
                        headers,
                        body,
                    } => {
                        debug!("{}", body.len());
                        let response_string = utils::build_response_string(status, &headers)?;

                        downguard.write_all(response_string.as_bytes()).await?;
                        debug!("Forwarded response header to downstream");

                        let mut downwriter = TlsWriter::new(&mut downguard);
                        downwriter.write_chunk(&body).await?;
                        downwriter.write_final_chunk().await?;
                        debug!("Forwarded response body to downstream");
                    }
                    HttpData::Request {
                        method,
                        url,
                        headers,
                        body,
                        version,
                    } => {
                        warn!("Received unexpected request from upstream client")
                    }
                    HttpData::BodyChunk {
                        stream_id,
                        data,
                        is_end,
                    } => {
                        warn!("Received unexpected chunk from upstream {subdomain} with stream ID {stream_id}");
                    }
                },
                Message::Data(d) => warn!(
                    "Received unexpected data of unexpected protocol type: {:?}",
                    d
                ),
                Message::StreamClose { stream_id } => {
                    warn!("Received unexpected stream close {stream_id} from upstream {subdomain}")
                }
                Message::HandshakeRequest {
                    supported_version,
                    supported_protocols,
                    auth_token,
                } => warn!("Received unexpected handshake request from upstream {subdomain}"),
                Message::HandshakeResponse {
                    accepted_version,
                    accepted_protocols,
                    subdomain,
                } => warn!("Received unexpected handshake response from upstream {subdomain}"),
                Message::Ping(v) => debug!("Received ping {v} from upstream {subdomain}"),
                Message::Pong(v) => debug!("Received pong {v} from upstream {subdomain}"),
                Message::Error(e) => error!("Message protocol error: {:?}", e),
            }
        }
    }

    async fn create_tls_config(&self) -> Result<ServerConfig> {
        let mut cert_contents = Vec::new();
        let mut cert_file = File::open(&self.config.cert_path).await?;
        cert_file.read_to_end(&mut cert_contents).await?;

        let mut key_contents = Vec::new();
        let mut key_file = File::open(&self.config.key_path).await?;
        key_file.read_to_end(&mut key_contents).await?;

        let mut cert_reader = std::io::BufReader::new(cert_contents.as_slice());
        let cert: Vec<_> = certs(&mut cert_reader).filter_map(|c| c.ok()).collect();

        let mut key_reader = std::io::BufReader::new(key_contents.as_slice());
        let key = rustls_pemfile::pkcs8_private_keys(&mut key_reader)
            .next()
            .expect("Error reading TLS key")?;

        let config = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(cert, PrivateKeyDer::Pkcs8(key))?;

        Ok(config)
    }

    fn generate_subdomain(&self) -> Result<String> {
        for _ in 0..10 {
            let subdomain = "todo".to_owned();

            if !self.upstream_clients.contains_key(&subdomain) {
                return Ok(subdomain);
            }
        }

        Err(anyhow::anyhow!(
            "Unable to generate unique subdomain after many tries"
        ))
    }
}
