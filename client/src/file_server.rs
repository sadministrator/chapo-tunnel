use std::{net::SocketAddr, path::PathBuf};

use anyhow::Result;
use axum::{
    body::StreamBody,
    extract::State,
    http::header,
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use mime_guess;
use tokio::fs::File;
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
    ) -> impl IntoResponse {
        let path = uri.path().trim_start_matches('/');
        let decoded_path = utils::decode_url(path);
        let file_path = root_path.join(decoded_path);

        if !file_path.starts_with(&root_path) {
            return Err(axum::http::StatusCode::FORBIDDEN);
        }

        match File::open(&file_path).await {
            Ok(file) => {
                let stream = ReaderStream::new(file);
                let body = StreamBody::new(stream);
                let mime_type = mime_guess::from_path(&file_path)
                    .first_or_octet_stream()
                    .to_string();
                let response = Response::builder()
                    .header(header::CONTENT_TYPE, mime_type)
                    .header(header::CONTENT_DISPOSITION, "inline")
                    .body(body)
                    .unwrap();

                Ok(response)
            }
            Err(_) => Err(axum::http::StatusCode::NOT_FOUND),
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
