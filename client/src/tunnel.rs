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
use tracing::{debug, info, trace};

use crate::utils::{self};

pub struct Config {
    file_server_port: u16,
    remote_host: String,
    remote_port: u16,
    supported_protocols: Vec<ProtocolType>,
    supported_version: String,
    auth_token: Option<String>,
}

impl Config {
    pub fn new(
        file_server_port: u16,
        remote_host: String,
        remote_port: u16,
        supported_protocols: Vec<ProtocolType>,
        supported_version: String,
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
            debug!("Handshake request sent");

            let response = protocol::read_message_locked(&tunnel_server).await?;
            debug!("Received handshake: {:?}", response);

            match response {
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
                        response
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
                    let (tx, rx) = mpsc::channel(32);
                    self.active_streams.insert(stream_id, tx);

                    let tunnel_server = tunnel_server.clone();
                    let _ = tokio::spawn(Self::handle_up_stream(
                        rx,
                        tunnel_server,
                        self.config.file_server_port,
                    ))
                    .await?;
                }
                Message::Data(data) => {
                    match data {
                        Data::Http(http_data) => match http_data {
                            HttpData::Request { url, .. } => {
                                let http_client = reqwest::Client::new();
                                let response = http_client
                                    .get(format!(
                                        "http://localhost:{}{}",
                                        self.config.file_server_port, url
                                    ))
                                    .send()
                                    .await?;
                                debug!("Received from file server {:?}", response);

                                let is_chunked = response
                                    .headers()
                                    .get("transfer-encoding")
                                    .map_or(false, |v| v.to_str().unwrap() == "chunked");
                                let content_len = response
                                    .headers()
                                    .get("content-length")
                                    .and_then(|v| v.to_str().unwrap().parse::<usize>().ok());
                                if !is_chunked
                                    || content_len.is_some_and(|len| len < STREAM_THRESHOLD)
                                {
                                    let response_message =
                                        Message::Data(Data::Http(HttpData::Response {
                                            status: response.status().into(),
                                            headers: response
                                                .headers()
                                                .into_iter()
                                                .map(|(k, v)| {
                                                    (k.to_string(), v.to_str().unwrap().to_owned())
                                                })
                                                .collect(),
                                            body: response.bytes().await?.to_vec(),
                                        }));

                                    debug!("Forwarding response to tunnel server");
                                    protocol::write_message_locked(
                                        &tunnel_server,
                                        &response_message,
                                    )
                                    .await?;
                                } else {
                                    let tunnel_server = tunnel_server.clone();
                                    let _ = tokio::spawn(async move {
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
                                                status: response.status().into(),
                                                headers: response
                                                    .headers()
                                                    .into_iter()
                                                    .map(|(k, v)| {
                                                        (
                                                            k.to_string(),
                                                            v.to_str().unwrap().to_owned(),
                                                        )
                                                    })
                                                    .collect(),
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
                                        let mut response_stream = response.bytes_stream();
                                        while let Some(Ok(chunk)) = response_stream.next().await {
                                            protocol::write_message_locked(
                                                &tunnel_server,
                                                &Message::http_chunk(
                                                    stream_id,
                                                    chunk.to_vec(),
                                                    false,
                                                ),
                                            )
                                            .await?
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
                                    .await?;
                                }
                            }
                            HttpData::Response {
                                status,
                                headers,
                                body,
                            } => todo!(),
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
                        },
                        _ => todo!(),
                    };
                }
                Message::StreamClose { stream_id } => {
                    self.active_streams
                        .remove(&stream_id)
                        .map(|(id, _)| debug!("Closed stream {id}"));
                }
                _ => todo!(),
            };
        }
    }

    async fn handle_up_stream(
        mut rx: Receiver<Message>,
        tunnel_server: Arc<Mutex<TcpStream>>,
        file_server_port: u16,
    ) -> Result<()> {
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
            Message::Data(Data::Http(HttpData::BodyChunk {
                stream_id,
                data,
                is_end,
            })) => Ok(data),
            _ => Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "Expected body chunk message",
            )),
        });
        let response = http_client
            .request(reqwest.method().clone(), file_server_url)
            .body(Body::wrap_stream(recv_stream))
            .send()
            .await?;
        debug!("Received response from file server: {:?}", response);

        let mut response_stream = response.bytes_stream();
        let stream_open = Message::stream_open(ProtocolType::Http);
        let Message::StreamOpen { stream_id, .. } = stream_open else {
            return Err(anyhow!("Expected stream open"));
        };
        debug!("Opening stream {stream_id} with tunnel server");
        protocol::write_message_locked(&tunnel_server, &stream_open).await?;

        while let Some(Ok(chunk)) = response_stream.next().await {
            protocol::write_message_locked(
                &tunnel_server,
                &Message::http_chunk(stream_id, chunk.to_vec(), false),
            )
            .await?
        }

        protocol::write_message_locked(
            &tunnel_server,
            &Message::http_chunk(stream_id, vec![], true),
        )
        .await?;
        debug!("Finished streaming response body {stream_id}");

        Ok(())
    }

    async fn handle_down_stream() -> Result<()> {
        todo!("refactor this logic out of other functions")
    }
}
