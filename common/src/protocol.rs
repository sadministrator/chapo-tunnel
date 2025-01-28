use std::{
    collections::HashMap,
    fmt, io,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use anyhow::{anyhow, Result};
use clap::ValueEnum;
use httparse;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use tokio::{
    io::{AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader},
    net::TcpStream,
    sync::Mutex,
};
use tokio_rustls::server::TlsStream;

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum Message {
    HandshakeRequest {
        supported_version: String,
        supported_protocols: Vec<ProtocolType>,
        auth_token: Option<String>,
    },
    HandshakeResponse {
        accepted_version: String,
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

#[derive(Clone, Debug)]
pub struct UpstreamClient {
    pub stream: Arc<Mutex<TcpStream>>,
    pub protocols: Vec<ProtocolType>,
}

impl UpstreamClient {
    pub fn new(stream: TcpStream, protocols: Vec<ProtocolType>) -> Self {
        Self {
            stream: Arc::new(Mutex::new(stream)),
            protocols,
        }
    }
}

#[derive(Clone, Debug)]
pub struct DownstreamClient {
    pub stream: Arc<Mutex<BufReader<TlsStream<TcpStream>>>>,
}

impl DownstreamClient {
    pub fn new(stream: BufReader<TlsStream<TcpStream>>) -> Self {
        let stream = Arc::new(Mutex::new(stream));

        Self { stream }
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
    Tcp(TcpData),
    Udp(UdpData),
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum HttpData {
    Request(HttpRequest),
    Response(HttpResponse),
    BodyChunk {
        stream_id: u32,
        data: Vec<u8>,
        is_end: bool,
    },
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct HttpRequest {
    pub method: String,
    pub path: String,
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
    pub version: u8,
}

impl HttpRequest {
    pub fn path_subdomain(&self) -> Result<String> {
        let host = self
            .headers
            .get("Host")
            .ok_or(anyhow!("Host missing from request"))?;
        let subdomain = host
            .split('.')
            .next()
            .ok_or(anyhow!("Subdomain missing from request"))?
            .to_owned();

        Ok(subdomain)
    }
}

#[derive(Serialize, Deserialize, Clone)]
pub struct HttpResponse {
    pub status: u16,
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
}

impl fmt::Debug for HttpResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpResponse")
            .field("status", &self.status)
            .field("headers", &self.headers)
            .field("body", &format!("[{} bytes]", self.body.len()))
            .finish()
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct TcpData {
    pub stream_id: u32,
    pub data: Vec<u8>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct UdpData {
    pub addr: String,
    pub port: u16,
    pub data: Vec<u8>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum Error {
    HandshakeError(String),
    ProtocolError(String),
    ConnectionError(String),
}

pub struct TlsWriter<'a> {
    inner: &'a mut BufReader<TlsStream<TcpStream>>,
}

impl<'a> TlsWriter<'a> {
    pub fn new(inner: &'a mut BufReader<TlsStream<TcpStream>>) -> Self {
        Self { inner }
    }
}

impl<'a> AsyncWrite for TlsWriter<'a> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

pub async fn write_message<T: Serialize>(stream: &mut TcpStream, message: &T) -> Result<()> {
    let serialized = bincode::serialize(message)?;
    let length = serialized.len() as u32;
    stream.write_all(&length.to_be_bytes()).await?;
    stream.write_all(&serialized).await?;
    stream.flush().await?;
    Ok(())
}

pub async fn read_message<T: DeserializeOwned>(stream: &mut TcpStream) -> Result<T> {
    let mut length_buf = [0u8; 4];
    stream.read_exact(&mut length_buf).await?;
    let length = u32::from_be_bytes(length_buf) as usize;

    let mut message_buf = vec![0u8; length];
    stream.read_exact(&mut message_buf).await?;
    Ok(bincode::deserialize(&message_buf)?)
}

pub fn parse_http_request(buf: &[u8]) -> Result<HttpRequest> {
    let mut headers = [httparse::EMPTY_HEADER; 64];
    let mut req = httparse::Request::new(&mut headers);

    let header_length = req.parse(buf)?.unwrap();
    let method = req.method.unwrap_or_default().to_owned();
    let path = req.path.unwrap_or_default().to_owned();
    let version = req.version.unwrap_or_default();

    let headers = req
        .headers
        .iter()
        .map(|h| {
            (
                h.name.to_string(),
                String::from_utf8_lossy(h.value).to_string(),
            )
        })
        .collect();

    let body = buf[header_length..].to_vec();

    Ok(HttpRequest {
        method,
        path,
        version,
        headers,
        body,
    })
}

pub fn request_is_complete(buf: &[u8]) -> Result<bool> {
    let mut headers = [httparse::EMPTY_HEADER; 64];
    let mut req = httparse::Request::new(&mut headers);

    Ok(req.parse(buf)?.is_complete())
}
