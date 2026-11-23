use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::io;
use std::pin::Pin;

use bytes::Bytes;
use hmac::{Hmac, KeyInit, Mac};
use http::header::{HOST, HeaderName, HeaderValue};
use http::{Method, Request, Uri};
use http_body_util::{BodyExt, Full};
use hyper::body::Body as _;
use hyper::client::conn::http1::SendRequest;
use hyper_util::rt::TokioIo;
use parking_lot::Mutex;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::time::{Duration, Instant};

/// How long a pooled webhook connection stays fit for reuse after its last attempt; an
/// attempt after that opens a new connection.
pub const IDLE_REUSE: Duration = Duration::from_secs(30);

/// A byte stream a webhook request is written to.
pub trait Io: AsyncRead + AsyncWrite + Send + Unpin + 'static {}

impl<T: AsyncRead + AsyncWrite + Send + Unpin + 'static> Io for T {}

pub type ConnectFuture = Pin<Box<dyn Future<Output = io::Result<Box<dyn Io>>> + Send>>;

/// Opens the connection a delivery attempt is written to. Tests pass one that serves an
/// in-process `hyper` service.
pub trait Connector: Send + Sync + 'static {
    fn connect(&self, uri: &Uri) -> ConnectFuture;
}

/// Plain HTTP over TCP; an `https` webhook URL fails the attempt.
#[derive(Debug, Default, Clone, Copy)]
pub struct TcpConnector;

impl Connector for TcpConnector {
    fn connect(&self, uri: &Uri) -> ConnectFuture {
        let target = match (uri.scheme_str(), uri.host()) {
            (Some("http"), Some(host)) => Ok((host.to_owned(), uri.port_u16().unwrap_or(80))),
            _ => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "only http webhook URLs are delivered",
            )),
        };
        Box::pin(async move {
            let stream = tokio::net::TcpStream::connect(target?).await?;
            Ok(Box::new(stream) as Box<dyn Io>)
        })
    }
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(char::from(DIGITS[usize::from(b >> 4)]));
        s.push(char::from(DIGITS[usize::from(b & 0xf)]));
    }
    s
}

pub(crate) fn signature_256(secret: &[u8], body: &[u8]) -> String {
    let mut mac = <Hmac<sha2::Sha256> as KeyInit>::new_from_slice(secret)
        .unwrap_or_else(|_| unreachable!("HMAC takes a key of any length"));
    mac.update(body);
    format!("sha256={}", hex(&mac.finalize().into_bytes()))
}

pub(crate) fn signature_1(secret: &[u8], body: &[u8]) -> String {
    let mut mac = <Hmac<sha1::Sha1> as KeyInit>::new_from_slice(secret)
        .unwrap_or_else(|_| unreachable!("HMAC takes a key of any length"));
    mac.update(body);
    format!("sha1={}", hex(&mac.finalize().into_bytes()))
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Outcome {
    pub status_code: i64,
    pub status: String,
    pub headers: BTreeMap<String, String>,
    pub body: Option<String>,
}

impl Outcome {
    pub(crate) fn failed(status: &str) -> Outcome {
        Outcome {
            status_code: 0,
            status: status.to_owned(),
            headers: BTreeMap::new(),
            body: None,
        }
    }
}

fn request(
    uri: &Uri,
    headers: &BTreeMap<String, String>,
    body: Bytes,
) -> Option<Request<Full<Bytes>>> {
    let target = uri.path_and_query().map_or("/", |p| p.as_str());
    let mut req = Request::builder()
        .method(Method::POST)
        .uri(target)
        .body(Full::new(body))
        .ok()?;
    let h = req.headers_mut();
    h.insert(HOST, HeaderValue::from_str(uri.authority()?.as_str()).ok()?);
    for (name, value) in headers {
        h.insert(
            HeaderName::from_bytes(name.as_bytes()).ok()?,
            HeaderValue::from_str(value).ok()?,
        );
    }
    Some(req)
}

async fn read_capped(mut body: hyper::body::Incoming, cap: usize) -> Option<(Vec<u8>, bool)> {
    let mut out = Vec::new();
    while out.len() < cap {
        let Some(frame) = body.frame().await else {
            return Some((out, true));
        };
        if let Ok(data) = frame.ok()?.into_data() {
            out.extend_from_slice(&data[..data.len().min(cap - out.len())]);
        }
    }
    let ended = body.is_end_stream();
    Some((out, ended))
}

type Sender = SendRequest<Full<Bytes>>;

pub(crate) struct Pool {
    idle: Mutex<HashMap<String, Vec<(Sender, Instant)>>>,
    per_origin: usize,
}

impl Pool {
    pub(crate) fn new(per_origin: usize) -> Pool {
        Pool {
            idle: Mutex::new(HashMap::new()),
            per_origin,
        }
    }

    fn take(&self, origin: &str) -> Option<Sender> {
        let mut idle = self.idle.lock();
        let senders = idle.get_mut(origin)?;
        while let Some((sender, since)) = senders.pop() {
            if sender.is_ready() && since.elapsed() < IDLE_REUSE {
                return Some(sender);
            }
        }
        None
    }

    fn give(&self, origin: String, sender: Sender) {
        let mut idle = self.idle.lock();
        idle.retain(|_, senders| {
            senders.retain(|(pooled, since)| pooled.is_ready() && since.elapsed() < IDLE_REUSE);
            !senders.is_empty()
        });
        let senders = idle.entry(origin).or_default();
        if senders.len() < self.per_origin && sender.is_ready() {
            senders.push((sender, Instant::now()));
        }
    }

    #[cfg(test)]
    pub(crate) fn idle(&self) -> usize {
        self.idle.lock().values().map(Vec::len).sum()
    }
}

async fn connect(connector: &dyn Connector, uri: &Uri) -> Option<Sender> {
    let io = connector.connect(uri).await.ok()?;
    let (sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(io))
        .await
        .ok()?;
    tokio::spawn(conn);
    Some(sender)
}

pub(crate) async fn send(
    connector: &dyn Connector,
    pool: &Pool,
    url: &str,
    headers: &BTreeMap<String, String>,
    body: Bytes,
    cap: usize,
) -> Outcome {
    let Ok(uri) = url.parse::<Uri>() else {
        return Outcome::failed("Invalid URL");
    };
    let Some(mut req) = request(&uri, headers, body) else {
        return Outcome::failed("Invalid URL");
    };
    let origin = format!(
        "{}://{}",
        uri.scheme_str().unwrap_or_default(),
        uri.authority().map_or("", |a| a.as_str())
    );
    let mut pooled = pool.take(&origin);
    let (mut sender, resp) = loop {
        let reused = pooled.is_some();
        let mut sender = match pooled.take() {
            Some(sender) => sender,
            None => match connect(connector, &uri).await {
                Some(sender) => sender,
                None => return Outcome::failed("Failed to connect"),
            },
        };
        match sender.try_send_request(req).await {
            Ok(resp) => break (sender, resp),
            Err(mut e) => match e.take_message() {
                Some(unsent) if reused => req = unsent,
                _ => return Outcome::failed("Invalid HTTP Response"),
            },
        }
    };
    let (parts, body) = resp.into_parts();
    let Some((body, ended)) = read_capped(body, cap).await else {
        return Outcome::failed("Invalid HTTP Response");
    };
    if ended && sender.ready().await.is_ok() {
        pool.give(origin, sender);
    }
    let code = parts.status.as_u16();
    Outcome {
        status_code: i64::from(code),
        status: if parts.status.is_success() {
            "OK".to_owned()
        } else {
            format!("Invalid HTTP Response: {code}")
        },
        headers: parts
            .headers
            .iter()
            .filter_map(|(k, v)| Some((k.as_str().to_owned(), v.to_str().ok()?.to_owned())))
            .collect(),
        body: (!body.is_empty()).then(|| String::from_utf8_lossy(&body).into_owned()),
    }
}
