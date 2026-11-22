use std::convert::Infallible;
use std::fmt::Write as _;

use bytes::Bytes;
use hmac::{Hmac, KeyInit, Mac};
use http::{HeaderMap, Response, StatusCode};
use http_body_util::{BodyExt, Collected, Full};
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto;
use serde_json::Value;
use sha2::Sha256;
use tokio::net::TcpListener;
use tokio::sync::{Mutex, mpsc};

/// One webhook request as the receiver got it.
#[derive(Debug, Clone)]
pub struct Delivery {
    pub path: String,
    pub headers: HeaderMap,
    pub body: Bytes,
}

impl Delivery {
    /// A header as text, empty when it is missing.
    #[must_use]
    pub fn header(&self, name: &str) -> &str {
        self.headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
    }

    /// # Panics
    ///
    /// When the body is not JSON.
    #[must_use]
    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body)
            .unwrap_or_else(|e| panic!("the delivery is not JSON: {e}"))
    }

    /// Whether `X-Hub-Signature-256` is `sha256=` and the HMAC SHA-256 of the body with `secret`.
    #[must_use]
    pub fn signed_with(&self, secret: &str) -> bool {
        let Some(hex) = self.header("x-hub-signature-256").strip_prefix("sha256=") else {
            return false;
        };
        let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(secret.as_bytes()) else {
            return false;
        };
        mac.update(&self.body);
        let want = mac
            .finalize()
            .into_bytes()
            .iter()
            .fold(String::new(), |mut s, b| {
                let _ = write!(s, "{b:02x}");
                s
            });
        want == hex
    }
}

/// A webhook receiver on its own port-0 listener. Every request answers `200` and queues a
/// [`Delivery`]; a test waits on [`Receiver::next`], never on time.
pub struct Receiver {
    url: String,
    deliveries: Mutex<mpsc::UnboundedReceiver<Delivery>>,
}

impl Receiver {
    /// # Panics
    ///
    /// When the listener cannot bind.
    pub async fn start() -> Receiver {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap_or_else(|e| panic!("bind the webhook receiver: {e}"));
        let addr = listener
            .local_addr()
            .unwrap_or_else(|e| panic!("read the receiver's address: {e}"));
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    continue;
                };
                let tx = tx.clone();
                let service =
                    hyper::service::service_fn(move |req: http::Request<hyper::body::Incoming>| {
                        let tx = tx.clone();
                        async move {
                            let (parts, body) = req.into_parts();
                            let body = body
                                .collect()
                                .await
                                .map_or_else(|_| Bytes::new(), Collected::to_bytes);
                            let _ = tx.send(Delivery {
                                path: parts.uri.path().to_owned(),
                                headers: parts.headers,
                                body,
                            });
                            let mut response = Response::new(Full::new(Bytes::new()));
                            *response.status_mut() = StatusCode::OK;
                            Ok::<_, Infallible>(response)
                        }
                    });
                tokio::spawn(async move {
                    let _ = auto::Builder::new(TokioExecutor::new())
                        .serve_connection(TokioIo::new(stream), service)
                        .await;
                });
            }
        });
        Receiver {
            url: format!("http://{addr}/webhook"),
            deliveries: Mutex::new(rx),
        }
    }

    /// The URL to set as an App's webhook.
    #[must_use]
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Waits for the next delivery.
    ///
    /// # Panics
    ///
    /// When the receiver task has stopped.
    pub async fn next(&self) -> Delivery {
        self.deliveries
            .lock()
            .await
            .recv()
            .await
            .unwrap_or_else(|| panic!("the webhook receiver stopped"))
    }

    /// Every delivery already received, without waiting.
    pub async fn received(&self) -> Vec<Delivery> {
        let mut rx = self.deliveries.lock().await;
        let mut out = Vec::new();
        while let Ok(d) = rx.try_recv() {
            out.push(d);
        }
        out
    }
}
