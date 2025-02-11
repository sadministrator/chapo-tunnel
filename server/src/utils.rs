use std::{
    collections::HashMap,
    io,
    pin::Pin,
    task::{Context, Poll},
};

use anyhow::{anyhow, Result};
use common::protocol::HttpData;
use httparse;
use tokio::{
    io::{AsyncWrite, AsyncWriteExt, BufStream},
    net::TcpStream,
};
use tokio_rustls::server::TlsStream;

pub struct TlsWriter<'a> {
    inner: &'a mut BufStream<TlsStream<TcpStream>>,
}

impl<'a> TlsWriter<'a> {
    pub fn new(inner: &'a mut BufStream<TlsStream<TcpStream>>) -> Self {
        Self { inner }
    }
}

impl<'a> TlsWriter<'a> {
    pub async fn write_simple_response(&mut self, response: &str) -> Result<()> {
        self.write_all(response.as_bytes()).await?;
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

pub fn parse_http_request(buf: &[u8]) -> Result<HttpData> {
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

    Ok(HttpData::Request {
        method,
        url: path,
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

pub fn extract_subdomain(headers: &HashMap<String, String>) -> Result<String> {
    let host = headers
        .get("Host")
        .ok_or(anyhow!("Host missing from request"))?;
    let subdomain = host
        .split('.')
        .next()
        .ok_or(anyhow!("Subdomain missing from request"))?
        .to_owned();

    Ok(subdomain)
}

pub fn build_response_string(
    status: u16,
    headers: &HashMap<String, String>,
    body: Vec<u8>,
) -> Result<String> {
    let mut response_string = format!("HTTP/1.1 {}\r\n", status);

    for (key, value) in headers {
        response_string.push_str(&format!("{}: {}\r\n", key, value));
    }
    response_string.push_str("\r\n");
    response_string.push_str(&String::from_utf8(body)?);

    Ok(response_string)
}
