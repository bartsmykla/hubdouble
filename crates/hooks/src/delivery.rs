use std::collections::BTreeMap;
use std::future::Future;
use std::io;
use std::pin::Pin;

use bytes::Bytes;
use hmac::{Hmac, KeyInit, Mac};
use http::header::{HOST, HeaderName, HeaderValue};
use http::{Method, Request, Uri};
use http_body_util::{BodyExt, Full};
use hyper_util::rt::TokioIo;
use tokio::io::{AsyncRead, AsyncWrite};

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

/// `X-Hub-Signature-256`: HMAC SHA-256 of the body under the secret.
pub(crate) fn signature_256(secret: &[u8], body: &[u8]) -> String {
    let mut mac = <Hmac<sha2::Sha256> as KeyInit>::new_from_slice(secret)
        .unwrap_or_else(|_| unreachable!("HMAC takes a key of any length"));
    mac.update(body);
    format!("sha256={}", hex(&mac.finalize().into_bytes()))
}

/// `X-Hub-Signature`: HMAC SHA-1 of the body under the secret.
pub(crate) fn signature_1(secret: &[u8], body: &[u8]) -> String {
    let mut mac = <Hmac<sha1::Sha1> as KeyInit>::new_from_slice(secret)
        .unwrap_or_else(|_| unreachable!("HMAC takes a key of any length"));
    mac.update(body);
    format!("sha1={}", hex(&mac.finalize().into_bytes()))
}

/// What one attempt got back.
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

/// Sends one `POST` and reads the whole answer. The caller bounds it with the attempt timeout.
pub(crate) async fn send(
    connector: &dyn Connector,
    url: &str,
    headers: &BTreeMap<String, String>,
    body: Bytes,
) -> Outcome {
    let Ok(uri) = url.parse::<Uri>() else {
        return Outcome::failed("Invalid URL");
    };
    let Some(req) = request(&uri, headers, body) else {
        return Outcome::failed("Invalid URL");
    };
    let Ok(io) = connector.connect(&uri).await else {
        return Outcome::failed("Failed to connect");
    };
    let Ok((mut sender, conn)) = hyper::client::conn::http1::handshake(TokioIo::new(io)).await
    else {
        return Outcome::failed("Failed to connect");
    };
    tokio::spawn(conn);
    let Ok(resp) = sender.send_request(req).await else {
        return Outcome::failed("Invalid HTTP Response");
    };
    let (parts, body) = resp.into_parts();
    let Ok(body) = body
        .collect()
        .await
        .map(http_body_util::Collected::to_bytes)
    else {
        return Outcome::failed("Invalid HTTP Response");
    };
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
