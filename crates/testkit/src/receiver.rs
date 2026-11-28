use std::convert::Infallible;
use std::fmt::Write as _;

use bytes::Bytes;
use hmac::{Hmac, KeyInit, Mac};
use http::{HeaderMap, Response, StatusCode};
use http_body_util::{BodyExt, Collected, Full};
use hubdouble_store::Store;
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto;
use serde_json::Value;
use sha2::Sha256;
use tokio::net::TcpListener;
use tokio::sync::{Mutex, mpsc};

use crate::coverage::Recorder;

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

    /// The payload: the JSON body, or the `payload` parameter of a form-encoded body.
    ///
    /// # Panics
    ///
    /// When the payload is not JSON.
    #[must_use]
    pub fn json(&self) -> Value {
        let json = crate::validate::delivery_json(self.header("content-type"), &self.body)
            .unwrap_or_else(|| panic!("the delivery has neither a JSON nor a form body"));
        serde_json::from_slice(&json).unwrap_or_else(|e| panic!("the delivery is not JSON: {e}"))
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

fn answer_status(query: Option<&str>) -> StatusCode {
    query
        .into_iter()
        .flat_map(|q| q.split('&'))
        .find_map(|pair| pair.strip_prefix("status="))
        .and_then(|code| code.parse::<u16>().ok())
        .and_then(|code| StatusCode::from_u16(code).ok())
        .unwrap_or(StatusCode::OK)
}

/// A webhook receiver on its own port-0 listener. Every request queues a [`Delivery`] and
/// answers `200`, or the code of a `status` query parameter of its URL, such as
/// `?status=404`; a test waits on [`Receiver::next`], never on time.
pub struct Receiver {
    url: String,
    deliveries: Mutex<mpsc::UnboundedReceiver<Delivery>>,
    coverage: Option<(Recorder, Store)>,
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
                            let status = answer_status(parts.uri.query());
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
                            *response.status_mut() = status;
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
            coverage: None,
        }
    }

    pub(crate) fn record_with(&mut self, recorder: Recorder, store: Store) {
        self.coverage = Some((recorder, store));
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
    /// When the receiver task has stopped, or the delivery differs from GitHub's docs
    /// ([`crate::validate::webhook_delivery`]).
    pub async fn next(&self) -> Delivery {
        let d = self
            .deliveries
            .lock()
            .await
            .recv()
            .await
            .unwrap_or_else(|| panic!("the webhook receiver stopped"));
        self.checked(d)
    }

    /// Every delivery already received, without waiting.
    ///
    /// # Panics
    ///
    /// When a delivery differs from GitHub's docs ([`crate::validate::webhook_delivery`]).
    pub async fn received(&self) -> Vec<Delivery> {
        let mut rx = self.deliveries.lock().await;
        let mut out = Vec::new();
        while let Ok(d) = rx.try_recv() {
            out.push(self.checked(d));
        }
        out
    }
    fn checked(&self, d: Delivery) -> Delivery {
        let payload = crate::validate::checked_webhook_delivery(&d.headers, &d.body)
            .unwrap_or_else(|errors| {
                panic!(
                    "the {} delivery differs from GitHub's docs:\n  {}\nbody: {}",
                    d.header("x-github-event"),
                    errors.join("\n  "),
                    String::from_utf8_lossy(&d.body)
                )
            });
        if let Some((recorder, store)) = &self.coverage {
            recorder.record_webhook(&d, &payload, store);
        }
        d
    }
}

impl Drop for Receiver {
    fn drop(&mut self) {
        if std::thread::panicking() {
            return;
        }
        while let Ok(d) = self.deliveries.get_mut().try_recv() {
            self.checked(d);
        }
    }
}
