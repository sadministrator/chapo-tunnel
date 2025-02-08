pub mod buffer {
    pub const DEFAULT_BUF_SIZE: usize = 1024;
    pub const MAX_SUBDOMAIN_ATTEMPTS: usize = 10;
}

pub mod network {
    pub const DEFAULT_BIND_ADDR: &str = "0.0.0.0";
    pub const HTTPS_LISTEN_PORT: &str = "443";
}
