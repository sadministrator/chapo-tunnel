use std::{sync::Arc, time::Duration};

use anyhow::{anyhow, Result};
use common::protocol::{self, Data, HttpData, Message, ProtocolType, STREAM_THRESHOLD};
use dashmap::DashMap;
use reqwest::Body;
use tokio::{
    net::TcpStream,
    sync::{
        mpsc::{self, Receiver, Sender},
        Mutex,
    },
};
use tokio_stream::{wrappers::ReceiverStream, StreamExt};
use tracing::{debug, error, info, trace, warn};

use crate::utils::{self};

pub struct Config {
    file_server_port: u16,
    remote_host: String,
    remote_port: u16,
    supported_protocols: Vec<ProtocolType>,
    supported_version: u32,
    auth_token: Option<String>,
}

impl Config {
    pub fn new(
        file_server_port: u16,
        remote_host: String,
        remote_port: u16,
        supported_protocols: Vec<ProtocolType>,
        supported_version: u32,
        auth_token: Option<String>,
    ) -> Self {
        Self {
            file_server_port,
            remote_host,
            remote_port,
            supported_protocols,
            supported_version,
            auth_token,
        }
    }
}

pub struct Client {
    config: Config,
    active_streams: DashMap<u32, Sender<Message>>,
}

impl Client {
    pub fn new(config: Config) -> Self {
        Self {
            config,
            active_streams: DashMap::new(),
        }
    }

    pub async fn run(self) -> Result<()> {
        const CHANNEL_BUF_SIZE: usize = 8192;

        let tunnel_server = TcpStream::connect(format!(
            "{}:{}",
            self.config.remote_host, self.config.remote_port
        ))
        .await?;
        info!("Connected to tunnel server {}", self.config.remote_host);

        let tunnel_server = Arc::new(Mutex::new(tunnel_server));
        let request = Message::HandshakeRequest {
            supported_version: self.config.supported_version.clone(),
            supported_protocols: self.config.supported_protocols.clone(),
            auth_token: self.config.auth_token.clone(),
        };

        loop {
            protocol::write_message_locked(&tunnel_server, &request).await?;
            debug!("Handshake request sent: {:?}", request);

            let hs_response = protocol::read_message_locked(&tunnel_server).await?;
            debug!("Received handshake: {:?}", hs_response);

            match hs_response {
                Message::HandshakeResponse {
                    accepted_version,
                    accepted_protocols,
                    subdomain,
                } => {
                    info!("Subdomain \"{subdomain}\" registered with tunnel server");
                    break;
                }
                _ => {
                    debug!(
                        "Received unexpected message from tunnel server: {:?}",
                        hs_response
                    );
                    tokio::time::sleep(Duration::new(1, 0)).await;
                }
            };
        }

        loop {
            let message = protocol::read_message_locked(&tunnel_server).await?;
            debug!("Received from tunnel server: {:?}", message);

            match message {
                Message::StreamOpen {
                    stream_id,
                    protocol,
                } => {
                    debug!("Stream opened by tunnel server with ID {stream_id}");
                    let (tx, rx) = mpsc::channel(CHANNEL_BUF_SIZE);
                    self.active_streams.insert(stream_id, tx);

                    let tunnel_server = tunnel_server.clone();
                    if let Err(e) = tokio::spawn(Self::handle_stream(
                        rx,
                        tunnel_server,
                        self.config.file_server_port,
                        stream_id,
                    ))
                    .await?
                    {
                        error!("Error handling stream: {e}");
                    };
                }
                Message::Data(data) => {
                    match data {
                        Data::Http(http_data) => match http_data {
                            HttpData::Request { headers, url, .. } => {
                                let http_client = reqwest::Client::new();
                                let force_stream = utils::is_video_request(&headers, &url);

                                let fs_response = http_client
                                    .get(format!(
                                        "http://localhost:{}{}",
                                        self.config.file_server_port, url
                                    ))
                                    .send()
                                    .await?;
                                debug!("Received from file server {:?}", fs_response);

                                let is_chunked = fs_response
                                    .headers()
                                    .get("transfer-encoding")
                                    .map_or(false, |v| v.to_str().unwrap() == "chunked");
                                let content_len = fs_response
                                    .headers()
                                    .get("content-length")
                                    .and_then(|v| v.to_str().unwrap().parse::<usize>().ok());

                                if force_stream
                                    || is_chunked
                                    || content_len.is_some_and(|len| len > STREAM_THRESHOLD)
                                {
                                    let tunnel_server = tunnel_server.clone();
                                    if let Err(e) = tokio::spawn(async move {
                                        let stream_open = Message::stream_open(ProtocolType::Http);
                                        let Message::StreamOpen { stream_id, .. } = stream_open
                                        else {
                                            return Err(anyhow!("Expected stream open"));
                                        };
                                        debug!("Opening stream {stream_id} with tunnel server");
                                        protocol::write_message_locked(
                                            &tunnel_server,
                                            &stream_open,
                                        )
                                        .await?;

                                        let response_header =
                                            Message::Data(Data::Http(HttpData::Response {
                                                status: fs_response.status().into(),
                                                headers: utils::add_video_headers(fs_response
                                                    .headers()
                                                    .into_iter()
                                                    .map(|(k, v)| {
                                                        (
                                                            k.to_string(),
                                                            v.to_str().unwrap().to_owned(),
                                                        )
                                                    })
                                                    .collect()),
                                                body: vec![],
                                            }));
                                        debug!(
                                            "Sending response header to tunnel server via {stream_id}"
                                        );
                                        trace!("{:?}", response_header);
                                        protocol::write_message_locked(
                                            &tunnel_server,
                                            &response_header,
                                        )
                                        .await?;

                                        trace!("Sending response body chunks to tunnel server via {stream_id}");
                                        let mut response_stream = fs_response.bytes_stream();
                                        while let Some(chunk_result) = response_stream.next().await {
                                            match chunk_result {
                                                Ok(chunk) => {
                                                    if let Err(e) = protocol::write_message_locked(
                                                        &tunnel_server,
                                                        &Message::http_chunk(stream_id, chunk.to_vec(), false),
                                                    ).await
                                                    {
                                                        error!("Failed to write chunk: {}", e);
                                                        break;
                                                    }
                                                }
                                                Err(e) => {
                                                    error!("Stream error: {}", e);
                                                    break;
                                                }
                                            }
                                        }

                                        protocol::write_message_locked(
                                            &tunnel_server,
                                            &Message::http_chunk(stream_id, vec![], true),
                                        )
                                        .await?;

                                        debug!("Closing stream {stream_id}");
                                        let stream_close = Message::stream_close(stream_id);
                                        protocol::write_message_locked(
                                            &tunnel_server,
                                            &stream_close,
                                        )
                                        .await?;

                                        Ok(())
                                    })
                                    .await? {
                                        error!("Error handling streaming request: {e}");
                                    };
                                } else {
                                    let response_message =
                                        Message::Data(Data::Http(HttpData::Response {
                                            status: fs_response.status().into(),
                                            headers: fs_response
                                                .headers()
                                                .into_iter()
                                                .map(|(k, v)| {
                                                    (k.to_string(), v.to_str().unwrap().to_owned())
                                                })
                                                .collect(),
                                            body: fs_response.bytes().await?.to_vec(),
                                        }));

                                    debug!("Forwarding response to tunnel server");
                                    protocol::write_message_locked(
                                        &tunnel_server,
                                        &response_message,
                                    )
                                    .await?;
                                }
                            }
                            HttpData::BodyChunk {
                                stream_id,
                                data,
                                is_end,
                            } => {
                                let chunk = Message::http_chunk(stream_id, data, is_end);
                                let Some(tx) = self.active_streams.get(&stream_id) else {
                                    debug!("Unexpected HTTP chunk with from {stream_id}");
                                    continue;
                                };
                                tx.send(chunk).await?;
                            }
                            HttpData::Response {
                                status,
                                headers,
                                body,
                            } => warn!("Received unexpected response from tunnel server"),
                        },
                        _ => todo!(),
                    };
                }
                Message::StreamClose { stream_id } => {
                    if let Some((id, tx)) = self.active_streams.remove(&stream_id) {
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        debug!("Closed stream {id}");
                    }
                }
                Message::Error(e) => match e {
                    protocol::Error::Stream(stream_id) => {
                        self.active_streams
                            .remove(&stream_id)
                            .map(|(id, _)| error!("Removed stream {id}"));
                    }
                    _ => todo!(),
                },
                _ => todo!(),
            };
        }
    }

    async fn handle_stream(
        mut rx: Receiver<Message>,
        tunnel_server: Arc<Mutex<TcpStream>>,
        file_server_port: u16,
        stream_id: u32,
    ) -> Result<()> {
        let result = async {
            debug!("Handling up stream");
            let Some(message) = rx.recv().await else {
                debug!("Stream channel has closed");
                return Ok(());
            };

            let Message::Data(Data::Http(HttpData::Request {
                method,
                url,
                headers,
                body,
                version,
            })) = message
            else {
                return Err(anyhow!("Expected an HTTP request"));
            };

            let http_client = reqwest::Client::new();
            let file_server_url = format!("http://localhost:{}{}", file_server_port, url);
            let reqwest = utils::to_reqwest(
                &http_client,
                HttpData::Request {
                    method,
                    url,
                    headers,
                    body,
                    version,
                },
            )
            .await?;
            let recv_stream = ReceiverStream::new(rx).map(|msg| match msg {
                Message::Data(Data::Http(HttpData::BodyChunk { data, .. })) => Ok(data),
                _ => Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "Expected body chunk message",
                )),
            });
            let fs_response = http_client
                .request(reqwest.method().clone(), file_server_url)
                .body(Body::wrap_stream(recv_stream))
                .send()
                .await?;
            debug!("Received response from file server: {:?}", fs_response);

            let response_header = HttpData::Response {
                status: fs_response.status().into(),
                headers: utils::add_video_headers(
                    fs_response
                        .headers()
                        .into_iter()
                        .map(|(k, v)| (k.to_string(), v.to_str().unwrap().to_owned()))
                        .collect(),
                ),
                body: vec![],
            };

            debug!(
                "Forwarding response header to tunnel server: {:?}",
                response_header
            );
            protocol::write_message_locked(&tunnel_server, &response_header).await?;

            let mut response_stream = fs_response.bytes_stream();
            let stream_open = Message::stream_open(ProtocolType::Http);
            let Message::StreamOpen { stream_id, .. } = stream_open else {
                return Err(anyhow!("Expected stream open"));
            };
            debug!("Opening stream {stream_id} with tunnel server");
            protocol::write_message_locked(&tunnel_server, &stream_open).await?;

            while let Some(chunk_result) = response_stream.next().await {
                match chunk_result {
                    Ok(chunk) => {
                        if let Err(e) = protocol::write_message_locked(
                            &tunnel_server,
                            &Message::http_chunk(stream_id, chunk.to_vec(), false),
                        )
                        .await
                        {
                            error!("Failed to write chunk: {}", e);
                            break;
                        }
                    }
                    Err(e) => {
                        error!("Stream error: {}", e);
                        break;
                    }
                }
            }

            protocol::write_message_locked(
                &tunnel_server,
                &Message::http_chunk(stream_id, vec![], true),
            )
            .await?;

            debug!("Closing stream {stream_id}");
            let stream_close = Message::stream_close(stream_id);
            protocol::write_message_locked(&tunnel_server, &stream_close).await?;

            Ok(())
        }
        .await;

        if let Err(e) = &result {
            error!("Stream error: {}", e);
            let _ = protocol::write_message_locked(
                &tunnel_server,
                &Message::Error(protocol::Error::Stream(stream_id)),
            )
            .await;
        }

        result
    }
}
