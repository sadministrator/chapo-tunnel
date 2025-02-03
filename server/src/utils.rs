use std::{
    io,
    pin::Pin,
    task::{Context, Poll},
};

use anyhow::{anyhow, Result};
use common::protocol::HttpRequest;
use httparse;
use tokio::{
    io::{AsyncWrite, AsyncWriteExt, BufReader},
    net::TcpStream,
};
use tokio_rustls::server::TlsStream;

pub struct TlsWriter<'a> {
    inner: &'a mut BufReader<TlsStream<TcpStream>>,
}

impl<'a> TlsWriter<'a> {
    pub fn new(inner: &'a mut BufReader<TlsStream<TcpStream>>) -> Self {
        Self { inner }
    }
}

impl<'a> TlsWriter<'a> {
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

pub fn extract_subdomain(request: &HttpRequest) -> Result<String> {
    let host = request
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
