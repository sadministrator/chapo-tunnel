use std::{net::SocketAddr, path::PathBuf};

use anyhow::Result;
use axum::{routing::get_service, Router};
use tower_http::{
    services::ServeDir,
    trace::{self, TraceLayer},
};
use tracing::{info, Level};

pub struct FileServer {
    path: PathBuf,
    pub port: u16,
}

impl FileServer {
    pub fn new(path: PathBuf, port: u16) -> Self {
        Self { path, port }
    }

    pub async fn run(self) -> Result<()> {
        let serve_dir = ServeDir::new(self.path.clone());
        let app = Router::new()
            .nest_service("/", get_service(serve_dir.clone()))
            .layer(
                TraceLayer::new_for_http()
                    .make_span_with(trace::DefaultMakeSpan::new().level(Level::DEBUG))
                    .on_response(trace::DefaultOnResponse::new().level(Level::INFO)),
            );
        let addr = SocketAddr::from(([127, 0, 0, 1], self.port));

        info!("Listening on {}, sharing {:?}", addr, serve_dir);

        axum::Server::bind(&addr)
            .serve(app.into_make_service())
            .await?;

        Ok(())
    }
}
