use std::{
    collections::HashMap,
    io,
    pin::Pin,
    task::{Context, Poll},
};

use anyhow::{anyhow, Result};
use common::protocol::{BodyReader, HttpRequest};
use httparse;
use tokio::{
    io::{AsyncWrite, AsyncWriteExt, BufStream},
    net::TcpStream,
};
use tokio_rustls::server::TlsStream;
use tracing::trace;

pub struct TlsWriter<'a> {
    inner: &'a mut BufStream<TlsStream<TcpStream>>,
}

impl<'a> TlsWriter<'a> {
    pub fn new(inner: &'a mut BufStream<TlsStream<TcpStream>>) -> Self {
        Self { inner }
    }
}

impl<'a> TlsWriter<'a> {
    pub async fn write_simple_response(&mut self, response: &[u8]) -> Result<()> {
        self.write_all(response).await?;
        self.flush().await?;
        Ok(())
    }

    pub async fn write_chunk(&mut self, data: &[u8]) -> Result<()> {
        if !data.is_empty() {
            let size_line = format!("{:x}\r\n", data.len());
            self.write_all(size_line.as_bytes()).await?;
            self.write_all(data).await?;
            self.write_all(b"\r\n").await?;
        }
        Ok(())
    }

    pub async fn write_final_chunk(&mut self) -> Result<()> {
        self.write_all(b"0\r\n\r\n").await?;
        self.flush().await?;
        Ok(())
    }

    pub async fn shutdown(&mut self) -> Result<()> {
        trace!("Starting TLS shutdown sequence");

        self.flush().await?;
        let tls_stream = self.inner.get_mut();

        match tokio::time::timeout(std::time::Duration::from_secs(5), tls_stream.shutdown()).await {
            Ok(Ok(_)) => {
                trace!("TLS shutdown completed successfully");
                Ok(())
            }
            Ok(Err(e)) => {
                trace!("TLS shutdown failed: {}", e);
                Err(anyhow!("TLS shutdown failed: {}", e))
            }
            Err(_) => {
                trace!("TLS shutdown timed out");
                Err(anyhow!("TLS shutdown timed out"))
            }
        }
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

pub fn parse_http_request(buf: &[u8]) -> Result<HttpRequest> {
    let mut headers = [httparse::EMPTY_HEADER; 64];
    let mut req = httparse::Request::new(&mut headers);

    let _header_length = req.parse(buf)?.unwrap();
    let method = req.method.unwrap_or_default().to_owned();
    let path = req.path.unwrap_or_default().to_owned();
    let version = req.version.unwrap_or_default();

    let headers: HashMap<String, String> = req
        .headers
        .iter()
        .map(|h| {
            (
                h.name.to_string(),
                String::from_utf8_lossy(h.value).to_string(),
            )
        })
        .collect();

    let body_reader = if is_chunked(&headers) {
        BodyReader::Chunked
    } else if let Some(length) = content_length(&headers) {
        BodyReader::Fixed(length)
    } else {
        BodyReader::Fixed(0)
    };

    Ok(HttpRequest {
        method,
        url: path,
        version,
        headers,
        body_reader,
    })
}

pub fn is_chunked(headers: &HashMap<String, String>) -> bool {
    if let Some(encoding) = headers.get("transfer-encoding") {
        if encoding == "chunked" {
            return true;
        }
    }

    false
}

pub fn content_length(headers: &HashMap<String, String>) -> Option<usize> {
    if let Some(length) = headers.get("content-length") {
        Some(length.parse().ok()?)
    } else {
        None
    }
}

pub fn request_is_complete(buf: &[u8]) -> Result<bool> {
    let mut headers = [httparse::EMPTY_HEADER; 64];
    let mut req = httparse::Request::new(&mut headers);

    Ok(req.parse(buf)?.is_complete())
}

pub fn build_response(status: u16, headers: &HashMap<String, String>, body: Vec<u8>) -> Vec<u8> {
    let mut response = Vec::new();

    response.extend_from_slice(format!("HTTP/1.1 {}\r\n", status).as_bytes());

    for (key, value) in headers {
        response.extend_from_slice(format!("{}: {}\r\n", key, value).as_bytes());
    }

    response.extend_from_slice(b"\r\n");
    response.extend_from_slice(&body);

    response
}
