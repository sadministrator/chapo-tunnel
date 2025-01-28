use std::{collections::HashSet, sync::Arc};

use anyhow::{anyhow, Result};
use common::protocol;
use dashmap::DashMap;
use rand::Rng;
use rustls_pemfile::{self, certs};
use tokio::{
    fs::File,
    io::{AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
};
use tokio_rustls::{
    rustls::{pki_types::PrivateKeyDer, ServerConfig},
    TlsAcceptor,
};
use tracing::{debug, error, info};

pub struct Config {
    pub domain: String,
    pub port: u16,

    pub require_auth: bool,
    pub auth_tokens: HashSet<String>,

    pub supported_protocols: Vec<protocol::ProtocolType>,

    pub cert_path: String,
    pub key_path: String,
}

pub struct Server {
    // subdomain -> upstream client
    pub upstream_clients: DashMap<String, protocol::UpstreamClient>,

    // stream ID -> downstream client
    pub downstream_clients: DashMap<u32, protocol::DownstreamClient>,

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

    pub async fn start(self: Arc<Self>) -> Result<()> {
        let upstream = self.clone();
        let downstream = self.clone();

        let upstream_handle = tokio::spawn(async move { upstream.listen_upstream().await });
        let downstream_handle =
            tokio::spawn(async move { downstream.listen_downstream_http().await });

        let _ = tokio::join!(upstream_handle, downstream_handle);

        Ok(())
    }

    async fn listen_upstream(&self) -> Result<()> {
        let listener = TcpListener::bind(format!("0.0.0.0:{}", self.config.port)).await?;
        info!(
            "Listening for upstream clients at port {}",
            self.config.port
        );

        while let Ok((mut stream, addr)) = listener.accept().await {
            info!("New upstream connection initiated by {addr}");

            let handshake: protocol::Message = protocol::read_message(&mut stream).await?;
            debug!("Received handshake: {:?}", handshake);

            match handshake {
                protocol::Message::HandshakeRequest {
                    supported_version,
                    supported_protocols,
                    auth_token,
                } => {
                    let accepted_protocols: Vec<protocol::ProtocolType> = supported_protocols
                        .into_iter()
                        .filter(|p| self.config.supported_protocols.contains(p))
                        .collect();
                    let subdomain = self.generate_subdomain()?;
                    let response = protocol::Message::HandshakeResponse {
                        accepted_protocols: accepted_protocols.clone(),
                        accepted_version: supported_version,
                        subdomain: subdomain.clone(),
                    };

                    protocol::write_message(&mut stream, &response).await?;
                    debug!("Handshake response sent");

                    let connection = protocol::UpstreamClient::new(stream, accepted_protocols);

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
        info!("Listening for HTTPS");

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
        upstream_clients: DashMap<String, protocol::UpstreamClient>,
        downstream_clients: DashMap<u32, protocol::DownstreamClient>,
    ) -> Result<()> {
        let tls_stream = acceptor.accept(downstream).await?;
        let tls_reader = BufReader::new(tls_stream);
        let stream_id = rand::thread_rng().gen();
        downstream_clients.insert(stream_id, protocol::DownstreamClient::new(tls_reader));

        loop {
            let downstream = downstream_clients
                .get(&stream_id)
                .ok_or(anyhow!("Couldn't find downstream for ID {stream_id}"))
                .unwrap();
            let mut downstream = downstream.stream.lock().await;

            let mut buf = vec![0u8; 1024];
            let n = downstream.read(&mut buf).await?;
            let request = protocol::parse_http_request(&buf[..n])?;
            debug!("Received request from downstream: {:?}", request);

            let subdomain = request.path_subdomain()?;

            let upstream = upstream_clients
                .get(&subdomain)
                .ok_or(anyhow!("No upstream found for subdomain \"{subdomain}\""))?;

            // todo: check whether request is large enough to stream, if so, create stream ID and send StreamOpen message to upstream
            // does the client already take care of doing this (by streaming request if it's large enough)? if so, what would the initial request look like?
            let message =
                protocol::Message::Data(protocol::Data::Http(protocol::HttpData::Request(request)));

            let mut upstream = upstream.stream.lock().await;
            protocol::write_message(&mut upstream, &message).await?;
            debug!("Sent upstream: {:?}", message);

            loop {
                let response: protocol::Message = protocol::read_message(&mut upstream).await?;
                debug!("Received from upstream: {:?}", response);

                match response {
                    protocol::Message::Data(protocol::Data::Http(http_data)) => match http_data {
                        protocol::HttpData::BodyChunk {
                            stream_id: chunk_id,
                            data,
                            is_end,
                        } => {
                            debug!("Received chunk from upstream {chunk_id}");
                            if chunk_id == stream_id {
                                let mut downwriter = protocol::TlsWriter::new(&mut downstream);
                                Self::write_chunk(&mut downwriter, &data).await?;

                                if is_end {
                                    downstream.write_all(b"0\r\n\r\n").await?;
                                    downstream.flush().await?;

                                    downstream_clients.remove(&stream_id);
                                    break;
                                }
                            }
                        }
                        protocol::HttpData::Response(http_response) => {
                            let mut response_string =
                                format!("HTTP/1.1 {}\r\n", http_response.status);
                            let is_streaming = http_response
                                .headers
                                .get("Transfer-Encoding")
                                .map(|v| v.to_lowercase().contains("chunked"))
                                .unwrap_or(false);

                            for (key, value) in &http_response.headers {
                                response_string.push_str(&format!("{}: {}\r\n", key, value));
                            }
                            response_string.push_str("\r\n");

                            downstream.write_all(response_string.as_bytes()).await?;

                            if !http_response.body.is_empty() {
                                if is_streaming {
                                    let mut downwriter = protocol::TlsWriter::new(&mut downstream);
                                    Self::write_chunk(&mut downwriter, &http_response.body).await?;
                                } else {
                                    downstream.write_all(&http_response.body).await?;
                                }
                            }

                            if !is_streaming {
                                downstream.flush().await?;
                                break;
                            }
                        }
                        protocol::HttpData::Request(http_request) => {
                            debug!("Received unexpected request from upstream client")
                        }
                    },
                    protocol::Message::StreamClose {
                        stream_id: closed_id,
                    } => {
                        if closed_id == stream_id {
                            downstream_clients.remove(&stream_id);
                            break;
                        }
                    }
                    _ => debug!("Upstream client response was malformed"),
                }
            }
        }
    }

    async fn write_chunk<W: AsyncWrite + Unpin>(writer: &mut W, data: &[u8]) -> Result<()> {
        if !data.is_empty() {
            let size_line = format!("{:x}\r\n", data.len());
            writer.write_all(size_line.as_bytes()).await?;
            writer.write_all(data).await?;
            writer.write_all(b"\r\n").await?;
        }

        Ok(())
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
