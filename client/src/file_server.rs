use std::{net::SocketAddr, path::PathBuf};

use anyhow::Result;
use axum::{
    body::StreamBody,
    extract::State,
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use tokio::fs::File;
use tokio_util::io::ReaderStream;
use tower_http::trace::{self, TraceLayer};
use tracing::{info, Level};

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
        let file_path = root_path.join(path);

        if !file_path.starts_with(&root_path) {
            return Err(axum::http::StatusCode::FORBIDDEN);
        }

        match File::open(&file_path).await {
            Ok(file) => {
                let stream = ReaderStream::new(file);
                let body = StreamBody::new(stream);
                Ok(Response::new(body))
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
