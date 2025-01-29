use anyhow::Result;
use common::protocol::{self, Data, HttpData, HttpResponse, Message, ProtocolType};
use tokio::net::TcpStream;
use tracing::{debug, info};

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
}

impl Client {
    pub fn new(config: Config) -> Self {
        Self { config }
    }

    pub async fn run(self) -> Result<()> {
        let mut tunnel_server = TcpStream::connect(format!(
            "{}:{}",
            self.config.remote_host, self.config.remote_port
        ))
        .await?;
        info!("Connected to tunnel server {}", self.config.remote_host);

        let request = Message::HandshakeRequest {
            supported_version: self.config.supported_version,
            supported_protocols: self.config.supported_protocols,
            auth_token: self.config.auth_token,
        };

        protocol::write_message(&mut tunnel_server, &request).await?;
        debug!("Handshake request sent");

        let response: Message = protocol::read_message(&mut tunnel_server).await?;
        debug!("Received handshake: {:?}", response);

        match response {
            Message::HandshakeResponse {
                accepted_version,
                accepted_protocols,
                subdomain,
            } => {
                info!("Subdomain \"{subdomain}\" registered with tunnel server");
            }
            _ => todo!(),
        };

        loop {
            let data: Message = protocol::read_message(&mut tunnel_server).await?;
            debug!("Received from tunnel server: {:?}", data);

            match data {
                Message::Data(Data::Http(http_data)) => {
                    match http_data {
                        HttpData::Request(http_request) => {
                            let http_client = reqwest::Client::new();
                            let http_response = http_client
                                .get(format!(
                                    "http://localhost:{}{}",
                                    self.config.file_server_port, http_request.path
                                ))
                                .send()
                                .await?;
                            debug!("Received from file server {:?}", http_response);

                            let http_message =
                                Message::Data(Data::Http(HttpData::Response(HttpResponse {
                                    status: http_response.status().into(),
                                    headers: http_response
                                        .headers()
                                        .into_iter()
                                        .map(|(k, v)| {
                                            (k.to_string(), v.to_str().unwrap().to_owned())
                                        })
                                        .collect(),
                                    body: http_response.bytes().await?.to_vec(),
                                })));

                            protocol::write_message(&mut tunnel_server, &http_message).await?;
                            debug!("Forwarded file to tunnel server");
                        }
                        HttpData::Response(HttpResponse {
                            status,
                            headers,
                            body,
                        }) => {
                            todo!("return 400 Bad Request")
                        }
                        HttpData::BodyChunk {
                            stream_id,
                            data,
                            is_end,
                        } => {
                            todo!("handle http chunks")
                        }
                    };
                }
                Message::Data(Data::Tcp(tcp_data)) => todo!(),
                Message::Data(Data::Udp(udp_data)) => todo!(),
                _ => todo!(),
            };
        }

        Ok(())
    }
}
