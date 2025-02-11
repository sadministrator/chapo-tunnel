use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use anyhow::{anyhow, Result};
use common::protocol::{
    self, Data, DownstreamClient, HttpData, Message, ProtocolType, Tunnel, UpstreamClient,
    STREAMING_THRESHOLD,
};
use dashmap::DashMap;
use tokio::{
    io::{AsyncReadExt, BufStream},
    net::{TcpListener, TcpStream},
    sync::Mutex,
};
use tokio_rustls::{server::TlsStream, TlsAcceptor};
use tracing::{debug, error, info, trace, warn};

use crate::{
    constants::{
        buffer::{DEFAULT_BUF_SIZE, MAX_SUBDOMAIN_ATTEMPTS},
        network::{DEFAULT_BIND_ADDR, HTTPS_LISTEN_PORT},
    },
    tls::TlsConfig,
    utils::{self, TlsWriter},
};

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

        let upstream_handle = tokio::spawn(async move { upstream.listen_upstream().await }); // 1
        let downstream_handle =
            tokio::spawn(async move { downstream.listen_downstream_http().await });

        let _ = tokio::join!(upstream_handle, downstream_handle);

        Ok(())
    }

    async fn listen_upstream(&self) -> Result<()> {
        let listener = self.create_upstream_listener().await?;

        while let Ok((stream, addr)) = listener.accept().await {
            info!("New upstream connection initiated by {addr}");
            let stream = Arc::new(Mutex::new(stream));

            self.handle_upstream_connection(stream, addr).await?;
        }

        Ok(())
    }

    async fn create_upstream_listener(&self) -> Result<TcpListener> {
        let bind_addr = format!("{DEFAULT_BIND_ADDR}:{}", self.config.upstream_listen);
        let listener = TcpListener::bind(&bind_addr).await?;
        info!(
            "Listening for upstream clients at port {}",
            self.config.upstream_listen
        );
        Ok(listener)
    }

    async fn handle_upstream_connection(
        &self,
        stream: Arc<Mutex<TcpStream>>,
        addr: std::net::SocketAddr,
    ) -> Result<()> {
        let handshake = protocol::read_message_locked(&stream).await?;
        debug!("Received handshake: {:?}", handshake);

        match handshake {
            Message::HandshakeRequest {
                supported_version,
                supported_protocols,
                auth_token,
            } => {
                self.process_handshake(stream, supported_version, supported_protocols, addr)
                    .await?;
            }
            _ => error!("Unable to connect to {addr}, handshake not received"),
        }
        Ok(())
    }

    async fn process_handshake(
        &self,
        stream: Arc<Mutex<TcpStream>>,
        supported_version: u32,
        supported_protocols: Vec<ProtocolType>,
        addr: std::net::SocketAddr,
    ) -> Result<()> {
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

        protocol::write_message_locked(&stream, &response).await?;
        debug!("Handshake response sent");

        let connection = UpstreamClient::new(stream, accepted_protocols);
        self.upstream_clients.insert(subdomain.clone(), connection);
        info!("{addr} added to upstream connections with subdomain `{subdomain}`");

        Ok(())
    }

    async fn listen_downstream_http(&self) -> Result<()> {
        let (listener, acceptor) = self.setup_downstream_listener().await?;

        while let Ok((stream, addr)) = listener.accept().await {
            info!("New HTTPS connection from: {addr}");
            self.spawn_downstream_handler(stream, acceptor.clone())
                .await;
        }
        Ok(())
    }

    async fn setup_downstream_listener(&self) -> Result<(TcpListener, TlsAcceptor)> {
        let https_bind_addr = format!("{DEFAULT_BIND_ADDR}:{HTTPS_LISTEN_PORT}");
        let tls_config = Arc::new(
            TlsConfig::new(self.config.cert_path.clone(), self.config.key_path.clone())
                .create_server_config()
                .await?,
        );
        let acceptor = TlsAcceptor::from(tls_config);
        let listener = TcpListener::bind(&https_bind_addr).await.map_err(|e| {
            let err = format!("Unable to bind to {https_bind_addr}: {e}");
            error!("{err}");
            anyhow!("{err}")
        })?;

        info!("Listening downstream for HTTPS");
        Ok((listener, acceptor))
    }

    async fn spawn_downstream_handler(&self, stream: TcpStream, acceptor: TlsAcceptor) {
        let upstream_clients = self.upstream_clients.clone();
        let tunnels = self.tunnels.clone();

        tokio::spawn(async move {
            if let Err(e) =
                Self::handle_connection(stream, acceptor, upstream_clients, tunnels).await
            {
                error!("HTTPS connection error: {e}");
            }
        });
    }

    async fn handle_connection(
        downstream: TcpStream,
        acceptor: TlsAcceptor,
        upstream_clients: DashMap<String, UpstreamClient>,
        tunnels: DashMap<u32, Tunnel>,
    ) -> Result<()> {
        let tls_stream = acceptor.accept(downstream).await?;
        let downstream_client = DownstreamClient::new(tls_stream);

        Self::process_downstream_requests(downstream_client, upstream_clients, tunnels).await
    }

    async fn process_downstream_requests(
        downstream_client: DownstreamClient,
        upstream_clients: DashMap<String, UpstreamClient>,
        tunnels: DashMap<u32, Tunnel>,
    ) -> Result<()> {
        let downstream = downstream_client.stream.clone();

        loop {
            let request = Self::read_http_request(downstream.clone()).await?;
            let HttpData::Request { headers, .. } = &request else {
                return Err(anyhow!("Expected HTTP request"));
            };
            let (subdomain, upstream_client) =
                Self::get_upstream_client(&request, &upstream_clients).await?;

            if Self::should_use_streaming(&headers) {
                tokio::spawn(Self::handle_streaming_request(
                    request,
                    downstream_client.clone(),
                    upstream_client.clone(),
                    tunnels.clone(),
                ));
            } else {
                Self::handle_simple_request(
                    &request,
                    upstream_client.clone(),
                    downstream_client.clone(),
                    tunnels.clone(),
                )
                .await?;
            }

            Self::process_upstream_response(
                downstream_client.clone(),
                upstream_client.clone(),
                &subdomain,
                &tunnels,
            )
            .await?;
        }
    }

    async fn read_http_request(
        stream: Arc<Mutex<BufStream<TlsStream<TcpStream>>>>,
    ) -> Result<HttpData> {
        let mut buf = vec![0u8; DEFAULT_BUF_SIZE];
        let n = stream.lock().await.read(&mut buf).await?;
        let request = utils::parse_http_request(&buf[..n])?;
        debug!("Received from downstream: {:?}", request);

        Ok(request)
    }

    async fn get_upstream_client(
        request: &HttpData,
        upstream_clients: &DashMap<String, UpstreamClient>,
    ) -> Result<(String, UpstreamClient)> {
        let HttpData::Request { headers, .. } = request else {
            return Err(anyhow!("Expected a HTTP request"));
        };

        let host = headers
            .get("Host")
            .ok_or_else(|| anyhow!("No Host header found"))?;

        let subdomain = host
            .split('.')
            .next()
            .ok_or_else(|| anyhow!("Invalid Host header format"))?
            .to_string();

        let client = upstream_clients
            .get(&subdomain)
            .ok_or_else(|| anyhow!("No upstream client found for subdomain: {}", subdomain))?
            .clone();

        Ok((subdomain, client))
    }

    fn should_use_streaming(headers: &HashMap<String, String>) -> bool {
        if let Some(content_length) = headers.get("Content-Length") {
            if let Ok(length) = content_length.parse::<usize>() {
                return length > STREAMING_THRESHOLD;
            }
        }

        if let Some(transfer_encoding) = headers.get("Transfer-Encoding") {
            return transfer_encoding.to_lowercase().contains("chunked");
        }

        false
    }

    async fn handle_streaming_request(
        request: HttpData,
        downstream_client: DownstreamClient,
        upstream_client: UpstreamClient,
        tunnels: DashMap<u32, Tunnel>,
    ) -> Result<()> {
        let stream_open = Message::stream_open(ProtocolType::Http);
        let Message::StreamOpen { stream_id, .. } = stream_open else {
            return Err(anyhow!("Expected steam open"));
        };
        let tunnel = Tunnel::new(upstream_client.clone(), downstream_client.clone());
        tunnels.insert(stream_id, tunnel);

        let stream_open = Message::stream_open(ProtocolType::Http);

        protocol::write_message_locked(&upstream_client.stream, &stream_open).await?;
        protocol::write_message_locked(
            &upstream_client.stream,
            &Message::Data(Data::Http(request)),
        )
        .await?;

        // todo: actually send chunks in a loop
        // probably need to refactor HTTP request parsing and HttpRequest struct to include a BodyReader enum to implement this

        let response = protocol::read_message_locked(&upstream_client.stream).await?;

        match response {
            Message::StreamOpen { stream_id, .. } => {
                Self::handle_streaming_response(
                    downstream_client,
                    upstream_client,
                    tunnels,
                    stream_id,
                )
                .await?
            }
            Message::Data(Data::Http(response)) => {
                Self::handle_simple_response(response, downstream_client).await?
            }
            _ => error!(
                "Unexpected response while handling streaming request: {:?}",
                response
            ),
        };

        Ok(())
    }

    async fn handle_simple_request(
        request: &HttpData,
        upstream_client: UpstreamClient,
        downstream_client: DownstreamClient,
        tunnels: DashMap<u32, Tunnel>,
    ) -> Result<()> {
        let message = Message::Data(Data::Http(request.clone()));

        debug!("Forwarding request upstream: {:?}", message);
        protocol::write_message_locked(&upstream_client.stream, &message).await?;

        let response_message = protocol::read_message_locked(&upstream_client.stream).await?;
        match response_message {
            Message::StreamOpen {
                stream_id,
                protocol,
            } => {
                Self::handle_streaming_response(
                    downstream_client.clone(),
                    upstream_client.clone(),
                    tunnels,
                    stream_id,
                )
                .await?
            }
            Message::Data(Data::Http(response)) => {
                Self::handle_simple_response(response, downstream_client).await?
            }
            _ => error!(
                "Unexpected response while handling simple request: {:?}",
                response_message
            ),
        };

        Ok(())
    }

    async fn process_upstream_response(
        downstream_client: DownstreamClient,
        upstream_client: UpstreamClient,
        subdomain: &str,
        tunnels: &DashMap<u32, Tunnel>,
    ) -> Result<()> {
        let response = protocol::read_message_locked(&upstream_client.stream).await?;

        match response {
            Message::StreamOpen {
                stream_id,
                protocol,
            } => {
                tunnels.insert(
                    stream_id,
                    Tunnel {
                        upstream: upstream_client.clone(),
                        downstream: downstream_client.clone(),
                    },
                );

                debug!("Spawned handle streaming response");
                if let Err(e) = tokio::spawn(Self::handle_streaming_response(
                    downstream_client,
                    upstream_client,
                    tunnels.clone(),
                    stream_id,
                ))
                .await?
                {
                    error!("Error handling streaming response: {e}");
                }
                debug!("Got past if");
            }
            Message::Data(Data::Http(response)) => {
                Self::handle_simple_response(response, downstream_client).await?;
            }
            message => {
                return Err(anyhow!(
                    "Unexpected message type from upstream client with domain `{}`: {:?}",
                    subdomain,
                    message
                ));
            }
        }

        Ok(())
    }

    async fn handle_streaming_response(
        downstream_client: DownstreamClient,
        upstream_client: UpstreamClient,
        tunnels: DashMap<u32, Tunnel>,
        stream_id: u32,
    ) -> Result<()> {
        let upstream = upstream_client.stream;
        let response_header = protocol::read_message_locked(&upstream).await?;
        debug!(
            "Received response header in {stream_id}: {:?}",
            response_header
        );

        let Message::Data(Data::Http(HttpData::Response {
            status, headers, ..
        })) = response_header
        else {
            return Err(anyhow!("Expected HTTP request header in {stream_id}"));
        };
        let response_string = utils::build_response_string(status, &headers, vec![])?;

        let mut downguard = downstream_client.stream.lock().await;
        let mut downwriter: TlsWriter<'_> = TlsWriter::new(&mut downguard);

        downwriter.write_simple_response(&response_string).await?;
        debug!(
            "Sent streaming response header to downstream: {:?}",
            response_string
        );

        loop {
            let stream_chunk = protocol::read_message_locked(&upstream).await?;
            trace!("{:?}", stream_chunk);

            match stream_chunk {
                Message::Data(Data::Http(HttpData::BodyChunk {
                    stream_id,
                    data,
                    is_end,
                })) => {
                    downwriter.write_chunk(&data).await?;

                    if is_end {
                        trace!("Writing final chunk to stream {stream_id}");
                        downwriter.write_final_chunk().await?;
                    }
                }
                Message::StreamClose { stream_id } => {
                    debug!("Upstream closed stream {stream_id}");
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

        Ok(())
    }

    async fn handle_simple_response(
        response: HttpData,
        downstream_client: DownstreamClient,
    ) -> Result<()> {
        let HttpData::Response {
            status,
            headers,
            body,
        } = response
        else {
            return Err(anyhow!("Expected simple HTTP response"));
        };
        let response_string = utils::build_response_string(status, &headers, body)?;

        let mut downguard = downstream_client.stream.lock().await;
        let mut downwriter = TlsWriter::new(&mut downguard);
        downwriter.write_simple_response(&response_string).await?;
        debug!(
            "Sent simple response header to downstream: {:?}",
            response_string
        );

        Ok(())
    }

    fn generate_subdomain(&self) -> Result<String> {
        for _ in 0..MAX_SUBDOMAIN_ATTEMPTS {
            let subdomain = "todo".to_owned();

            if !self.upstream_clients.contains_key(&subdomain) {
                return Ok(subdomain);
            }
        }

        Err(anyhow::anyhow!(
            "Unable to generate unique subdomain after maximum number of tries"
        ))
    }
}
