use anyhow::Result;
use tokio::fs::File;
use tokio::io::AsyncReadExt;
use tokio_rustls::rustls::{
    pki_types::{CertificateDer, PrivateKeyDer},
    server::ServerConfig,
};

#[derive(Clone)]
pub struct TlsConfig {
    cert_path: String,
    key_path: String,
}

impl TlsConfig {
    pub fn new(cert_path: String, key_path: String) -> Self {
        Self {
            cert_path,
            key_path,
        }
    }

    pub async fn create_server_config(&self) -> Result<ServerConfig> {
        let cert_contents = Self::read_file(&self.cert_path).await?;
        let key_contents = Self::read_file(&self.key_path).await?;

        let cert = Self::parse_certificates(cert_contents)?;
        let key = Self::parse_private_key(key_contents)?;

        Ok(ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(cert, key)?)
    }

    async fn read_file(path: &str) -> Result<Vec<u8>> {
        let mut contents = Vec::new();
        let mut file = File::open(path).await?;
        file.read_to_end(&mut contents).await?;
        Ok(contents)
    }

    fn parse_certificates(contents: Vec<u8>) -> Result<Vec<CertificateDer<'static>>> {
        let mut reader = std::io::BufReader::new(contents.as_slice());
        Ok(rustls_pemfile::certs(&mut reader)
            .filter_map(|c| c.ok())
            .collect())
    }

    fn parse_private_key(contents: Vec<u8>) -> Result<PrivateKeyDer<'static>> {
        let mut reader = std::io::BufReader::new(contents.as_slice());
        let key = PrivateKeyDer::Pkcs8(
            rustls_pemfile::pkcs8_private_keys(&mut reader)
                .next()
                .expect("Error reading TLS key")?,
        );
        Ok(key)
    }
}
