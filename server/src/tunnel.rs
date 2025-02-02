use std::{collections::HashSet, sync::Arc};

use anyhow::{anyhow, Result};
use common::protocol::{
    self, Data, DownstreamClient, HttpData, Message, ProtocolType, UpstreamClient,
};
use dashmap::DashMap;
use rand::Rng;
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
use tracing::{debug, error, info};

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

    // stream ID -> downstream client
    pub downstream_clients: DashMap<u32, DownstreamClient>,

    pub config: Config,
}

impl Server {
    pub fn new(config: Config) -> Self {
        let upstream_clients = DashMap::new();
        let downstream_clients = DashMap::new();

        Self {
            upstream_clients,
            downstream_clients,
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

            let handshake: Message = protocol::read_message(&mut stream).await?;
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
            let downstream_clients = self.downstream_clients.clone();

            tokio::spawn(async move {
                if let Err(e) =
                    Self::handle_connection(stream, acceptor, upstream_clients, downstream_clients)
                        .await
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
        downstream_clients: DashMap<u32, DownstreamClient>,
    ) -> Result<()> {
        const STREAM_THRESHOLD: usize = 1024;

        let tls_stream = acceptor.accept(downstream).await?;
        let tls_reader = BufReader::new(tls_stream);
        let downstream = DownstreamClient::new(tls_reader);

        loop {
            let mut downstream = downstream.stream.lock().await;
            let mut buf = vec![0u8; 1024];
            let n = downstream.read(&mut buf).await?;
            let request = utils::parse_http_request(&buf[..n])?;
            debug!("Received from downstream: {:?}", request);

            let subdomain = request.path_subdomain()?;
            let upstream = upstream_clients
                .get(&subdomain)
                .ok_or(anyhow!("No upstream found for subdomain \"{subdomain}\""))?;
            let mut upstream = upstream.stream.lock().await;

            let is_chunked = request
                .headers
                .get("transfer-encoding")
                .map_or(false, |v| v.eq_ignore_ascii_case("chunked"));
            let content_len = request
                .headers
                .get("content-length")
                .and_then(|v| v.parse::<usize>().ok());

            if !is_chunked || content_len.is_some_and(|len| len < STREAM_THRESHOLD) {
                let message = Message::Data(Data::Http(HttpData::Request(request)));

                protocol::write_message(&mut upstream, &message).await?;
                debug!("Forwarded request upstream: {:?}", message);
            } else {
                let stream_id: u32 = rand::thread_rng().gen();
                let stream_open: Message = Message::StreamOpen {
                    stream_id,
                    protocol: ProtocolType::Http,
                };
                protocol::write_message(&mut upstream, &stream_open).await?;

                loop {
                    let mut buf = [0u8; 1024];
                    let n = downstream.read(&mut buf).await?;
                    let is_end = n == 0;
                    let chunk = Message::Data(Data::Http(HttpData::BodyChunk {
                        stream_id,
                        data: buf[..n].to_vec(),
                        is_end,
                    }));
                    protocol::write_message(&mut upstream, &chunk).await?;

                    if is_end {
                        break;
                    }
                }
            }

            let response: Message = protocol::read_message(&mut upstream).await?;
            debug!("Received from upstream: {:?}", response);

            match response {
                Message::StreamOpen {
                    stream_id,
                    protocol,
                } => {
                    debug!("Upstream at {subdomain} opened stream {stream_id}");
                    loop {
                        let stream_chunk: Message = protocol::read_message(&mut upstream).await?;
                        match stream_chunk {
                            Message::Data(Data::Http(HttpData::BodyChunk {
                                stream_id,
                                mut data,
                                is_end,
                            })) => {
                                let mut downwriter = TlsWriter::new(&mut downstream);
                                downwriter.write(&data).await?;

                                if is_end {
                                    downwriter.flush().await?;
                                }
                            }
                            Message::StreamClose { stream_id } => {
                                debug!("Upstream at {subdomain} closed stream {stream_id}");
                                break;
                            }
                            _ => {
                                error!(
                                    "Received unexpected message from upstream: {:?}",
                                    stream_chunk
                                );

                                let stream_close = Message::StreamClose { stream_id };
                                protocol::write_message(&mut upstream, &stream_close).await?;
                                break;
                            }
                        }
                    }
                }
                Message::Data(Data::Http(http_data)) => match http_data {
                    HttpData::Response(http_response) => {
                        let mut response_string = format!("HTTP/1.1 {}\r\n", http_response.status);
                        let is_chunked = http_response
                            .headers
                            .get("transfer-encoding")
                            .map_or(false, |v| v.eq_ignore_ascii_case("chunked"));

                        for (key, value) in &http_response.headers {
                            response_string.push_str(&format!("{}: {}\r\n", key, value));
                        }
                        response_string.push_str("\r\n");

                        downstream.write_all(response_string.as_bytes()).await?;
                        debug!("Sent response header to downstream");

                        let mut downwriter = TlsWriter::new(&mut downstream);

                        if !http_response.body.is_empty() {
                            if is_chunked {
                                downwriter.write_chunk(&http_response.body).await?;
                            } else {
                                downwriter.write_all(&http_response.body).await?;
                            }
                        }

                        if is_chunked {
                            downwriter.write_final_chunk().await?;
                        } else {
                            downstream.flush().await?;
                        }
                    }
                    HttpData::Request(http_request) => {
                        error!("Received unexpected request from upstream client")
                    }
                    HttpData::BodyChunk {
                        stream_id,
                        data,
                        is_end,
                    } => {
                        error!("Received unexpected chunk from upstream {subdomain} with stream ID {stream_id}");

                        let stream_close = Message::StreamClose { stream_id };
                        protocol::write_message(&mut upstream, &stream_close).await?;
                    }
                },
                Message::Data(d) => error!(
                    "Received unexpected data of unexpected protocol type: {:?}",
                    d
                ),
                Message::StreamClose {
                    stream_id: closed_id,
                } => error!("Received unexpected stream close from upstream {subdomain}"),
                Message::HandshakeRequest {
                    supported_version,
                    supported_protocols,
                    auth_token,
                } => error!("Received unexpected handshake request from upstream {subdomain}"),
                Message::HandshakeResponse {
                    accepted_version,
                    accepted_protocols,
                    subdomain,
                } => error!("Received unexpected handshake response from upstream {subdomain}"),
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
