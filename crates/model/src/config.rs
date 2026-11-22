use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::time::Duration;

/// The parsed `HUBDOUBLE_*` environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub addr: SocketAddr,
    /// Origin of every URL in an answer or payload except links to resources, without a trailing `/`.
    pub base_url: String,
    /// Origin of `html_url` and other links to resources, without a trailing `/`.
    pub web_url: String,
    pub oidc_issuer: String,
    pub admin_token: String,
    pub seed: Option<PathBuf>,
    /// Tokio worker threads; `None` means the CPU count.
    pub workers: Option<usize>,
    pub max_body_bytes: usize,
    /// How long shutdown waits for open connections before it closes them.
    pub shutdown_grace: Duration,
}

impl Default for Config {
    fn default() -> Config {
        let base_url = "http://localhost:8000".to_owned();
        Config {
            addr: SocketAddr::from((Ipv4Addr::UNSPECIFIED, 8000)),
            web_url: base_url.clone(),
            oidc_issuer: format!("{base_url}/_actions"),
            base_url,
            admin_token: "hubdouble-admin".to_owned(),
            seed: None,
            workers: None,
            max_body_bytes: 100 * 1024 * 1024,
            shutdown_grace: Duration::from_secs(10),
        }
    }
}
