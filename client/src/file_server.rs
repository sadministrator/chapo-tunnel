use std::{io::SeekFrom, net::SocketAddr, path::PathBuf};

use anyhow::Result;
use axum::{
    body::StreamBody,
    extract::State,
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use mime_guess;
use tokio::{
    fs::File,
    io::{AsyncReadExt, AsyncSeekExt},
};
use tokio_util::io::ReaderStream;
use tower_http::trace::{self, TraceLayer};
use tracing::{info, Level};

use crate::utils;

pub struct FileServer {
    path: PathBuf,
    pub port: u16,
}

impl FileServer {
    pub fn new(path: PathBuf, port: u16) -> Self {
        Self { path, port }
    }

    async fn stream_file(
        State(root_path): State<PathBuf>,
        uri: axum::http::Uri,
        headers: axum::http::HeaderMap,
    ) -> impl IntoResponse {
        let path = uri.path().trim_start_matches('/');
        let decoded_path = utils::decode_url(path);
        let file_path = root_path.join(decoded_path);
        let mime_type = mime_guess::from_path(&file_path)
            .first_or_octet_stream()
            .to_string();

        if !file_path.starts_with(&root_path) {
            return Err(StatusCode::FORBIDDEN);
        }

        match File::open(&file_path).await {
            Ok(mut file) => {
                let file_size = file
                    .metadata()
                    .await
                    .map_err(|_| StatusCode::NOT_FOUND)?
                    .len();
                let range = if let Some(range) = headers.get(header::RANGE) {
                    Self::parse_range_header(
                        range.to_str().map_err(|_| StatusCode::BAD_REQUEST)?,
                        file_size,
                    )
                } else {
                    None
                };

                match range {
                    Some((start, end)) => {
                        file.seek(SeekFrom::Start(start))
                            .await
                            .map_err(|_| StatusCode::BAD_REQUEST)?;
                        let content_length = end - start + 1;

                        let limited_reader = file.take(content_length);
                        let stream = ReaderStream::new(limited_reader);
                        let body = StreamBody::new(stream);

                        let response = Response::builder()
                            .status(StatusCode::PARTIAL_CONTENT)
                            .header(header::CONTENT_TYPE, mime_type)
                            .header(header::CONTENT_LENGTH, content_length)
                            .header(
                                header::CONTENT_RANGE,
                                format!("bytes {}-{}/{}", start, end, file_size),
                            )
                            .header(header::ACCEPT_RANGES, "bytes")
                            .header(header::CONTENT_DISPOSITION, "inline")
                            .body(body)
                            .map_err(|_| StatusCode::BAD_REQUEST)?;

                        Ok(response)
                    }
                    None => {
                        let limited_reader = file.take(file_size);
                        let stream = ReaderStream::new(limited_reader);
                        let body = StreamBody::new(stream);

                        let response = Response::builder()
                            .status(StatusCode::OK)
                            .header(header::CONTENT_TYPE, mime_type)
                            .header(header::CONTENT_LENGTH, file_size)
                            .header(header::ACCEPT_RANGES, "bytes")
                            .header(header::CONTENT_DISPOSITION, "inline")
                            .body(body)
                            .map_err(|_| StatusCode::BAD_REQUEST)?;

                        Ok(response)
                    }
                }
            }
            Err(_) => Err(axum::http::StatusCode::NOT_FOUND),
        }
    }

    fn parse_range_header(range_header: &str, file_size: u64) -> Option<(u64, u64)> {
        if !range_header.starts_with("bytes=") {
            return None;
        }

        let ranges_str = &range_header["bytes=".len()..];
        let range = ranges_str.split(',').next()?;
        let mut range_parts = range.split('-');

        let start = range_parts.next()?.parse::<u64>().ok()?;

        let end = match range_parts.next() {
            Some(end_str) if !end_str.is_empty() => end_str.parse::<u64>().ok()?,
            _ => file_size - 1,
        };

        if start <= end && end < file_size {
            Some((start, end))
        } else {
            None
        }
    }

    pub async fn run(self) -> Result<()> {
        let app = Router::new()
            .route("/*path", get(Self::stream_file))
            .with_state(self.path.clone())
            .layer(
                TraceLayer::new_for_http()
                    .make_span_with(trace::DefaultMakeSpan::new().level(Level::DEBUG))
                    .on_response(trace::DefaultOnResponse::new().level(Level::INFO)),
            );
        let addr = SocketAddr::from(([127, 0, 0, 1], self.port));

        info!("Listening on {}, sharing {:?}", addr, self.path);

        axum::Server::bind(&addr)
            .serve(app.into_make_service())
            .await?;

        Ok(())
    }
}
