use std::{collections::HashMap, fmt, sync::Arc};

use anyhow::Result;
use clap::ValueEnum;
use rand::Rng;
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt, BufStream},
    net::TcpStream,
    sync::Mutex,
};
use tokio_rustls::server::TlsStream;

pub const STREAMING_THRESHOLD: usize = 1024;

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum Message {
    HandshakeRequest {
        supported_version: u32,
        supported_protocols: Vec<ProtocolType>,
        auth_token: Option<String>,
    },
    HandshakeResponse {
        accepted_version: u32,
        accepted_protocols: Vec<ProtocolType>,
        subdomain: String,
    },
    StreamOpen {
        stream_id: u32,
        protocol: ProtocolType,
    },
    StreamClose {
        stream_id: u32,
    },
    Ping(u64),
    Pong(u64),
    Data(Data),
    Error(Error),
}

impl Message {
    pub fn stream_open(protocol: ProtocolType) -> Self {
        Self::StreamOpen {
            stream_id: rand::rng().random(),
            protocol,
        }
    }

    pub fn stream_close(stream_id: u32) -> Self {
        Self::StreamClose { stream_id }
    }

    pub fn http_chunk(stream_id: u32, data: Vec<u8>, is_end: bool) -> Self {
        Self::Data(Data::Http(HttpData::BodyChunk {
            stream_id,
            data,
            is_end,
        }))
    }
}

#[derive(Clone, Debug)]
pub struct UpstreamClient {
    pub stream: Arc<Mutex<TcpStream>>,
    pub protocols: Vec<ProtocolType>,
}

impl UpstreamClient {
    pub fn new(stream: Arc<Mutex<TcpStream>>, protocols: Vec<ProtocolType>) -> Self {
        Self { stream, protocols }
    }
}

#[derive(Clone, Debug)]
pub struct DownstreamClient {
    pub stream: Arc<Mutex<BufStream<TlsStream<TcpStream>>>>,
}

impl DownstreamClient {
    pub fn new(tls_reader: TlsStream<TcpStream>) -> Self {
        Self {
            stream: Arc::new(Mutex::new(BufStream::new(tls_reader))),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Tunnel {
    pub upstream: UpstreamClient,
    pub downstream: DownstreamClient,
}

impl Tunnel {
    pub fn new(upstream: UpstreamClient, downstream: DownstreamClient) -> Self {
        Self {
            upstream,
            downstream,
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, ValueEnum, PartialEq)]
pub enum ProtocolType {
    Http,
    Tcp,
    Udp,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum Data {
    Http(HttpData),
    Tcp {
        stream_id: u32,
        data: Vec<u8>,
    },
    Udp {
        addr: String,
        port: u16,
        data: Vec<u8>,
    },
}

#[derive(Serialize, Deserialize, Clone)]
pub enum HttpData {
    Request {
        method: String,
        url: String,
        headers: HashMap<String, String>,
        body: Vec<u8>,
        version: u8,
    },
    Response {
        status: u16,
        headers: HashMap<String, String>,
        body: Vec<u8>,
    },
    BodyChunk {
        stream_id: u32,
        data: Vec<u8>,
        is_end: bool,
    },
}

impl fmt::Debug for HttpData {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HttpData::Request {
                method,
                url,
                headers,
                body,
                version,
            } => {
                write!(
                f,
                "Request {{ method: {:?}, url: {:?}, headers: {:?}, body: {:.3?} kB, version: {:?} }}",
                method, url, headers, body.len() as f32 / 1024.0, version
            )
            }
            HttpData::Response {
                status,
                headers,
                body,
            } => write!(
                f,
                "Response {{ status: {:?}, headers: {:?}, body: {:.3?} kB }}",
                status,
                headers,
                body.len() as f32 / 1024.0
            ),
            HttpData::BodyChunk {
                stream_id,
                data,
                is_end,
            } => write!(
                f,
                "BodyChunk {{ stream_id: {:?}, data: {:.3?} kB, is_end: {:?} }}",
                stream_id,
                data.len() as f32 / 1024.0,
                is_end
            ),
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum Error {
    Handshake(String),
    Protocol(String),
    Connection(String),
    Stream(u32),
}

pub async fn write_message<W>(stream: &mut W, message: &Message) -> Result<()>
where
    W: AsyncWriteExt + Unpin,
{
    let serialized = bincode::serialize(message)?;
    let length = serialized.len() as u32;
    stream.write_all(&length.to_be_bytes()).await?;
    stream.write_all(&serialized).await?;
    stream.flush().await?;
    Ok(())
}

pub async fn read_message<R>(stream: &mut R) -> Result<Message>
where
    R: AsyncReadExt + Unpin,
{
    let mut length_buf = [0u8; 4];
    stream.read_exact(&mut length_buf).await?;
    let length = u32::from_be_bytes(length_buf) as usize;

    let mut message_buf = vec![0u8; length];
    stream.read_exact(&mut message_buf).await?;
    Ok(bincode::deserialize(&message_buf)?)
}

pub async fn write_message_locked(stream: &Arc<Mutex<TcpStream>>, message: &Message) -> Result<()> {
    let mut guard = stream.lock().await;
    write_message(&mut *guard, message).await
}

pub async fn read_message_locked(stream: &Arc<Mutex<TcpStream>>) -> Result<Message> {
    let mut guard = stream.lock().await;
    read_message(&mut *guard).await
}
