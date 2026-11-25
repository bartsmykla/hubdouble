use bytes::Bytes;
use http::header::{CONTENT_TYPE, LOCATION};
use http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use hubdouble_model::{ApiError, Timestamp};
use serde::Serialize;

pub const JSON: &str = "application/json; charset=utf-8";

const BODY_CAPACITY: usize = 16 * 1024;

/// A handler's answer. The server adds the common headers, the `ETag` and the request log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Bytes,
    pub modified: Option<Timestamp>,
}

impl Reply {
    /// `200` with `value` as JSON.
    ///
    /// # Errors
    ///
    /// `500` when `value` cannot be serialized.
    pub fn json<T: Serialize + ?Sized>(value: &T) -> Result<Reply, ApiError> {
        let mut body = Vec::with_capacity(BODY_CAPACITY);
        serde_json::to_writer(&mut body, value).map_err(|e| ApiError::internal(e.to_string()))?;
        Ok(Reply::raw(JSON, body))
    }

    /// `200` with `body` as `content_type`.
    #[must_use]
    pub fn raw(content_type: &'static str, body: impl Into<Bytes>) -> Reply {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static(content_type));
        Reply {
            status: StatusCode::OK,
            headers,
            body: body.into(),
            modified: None,
        }
    }

    /// `302` to `location`.
    ///
    /// # Errors
    ///
    /// `500` when `location` is not a valid header value.
    pub fn redirect(location: &str) -> Result<Reply, ApiError> {
        let mut headers = HeaderMap::new();
        headers.insert(LOCATION, header_value(location)?);
        Ok(Reply {
            status: StatusCode::FOUND,
            headers,
            body: Bytes::new(),
            modified: None,
        })
    }

    /// `204` without a body.
    #[must_use]
    pub fn empty() -> Reply {
        Reply {
            status: StatusCode::NO_CONTENT,
            headers: HeaderMap::new(),
            body: Bytes::new(),
            modified: None,
        }
    }

    /// The same reply with another status, such as `201` for a create.
    #[must_use]
    pub fn status(mut self, status: StatusCode) -> Reply {
        self.status = status;
        self
    }

    /// The same reply with one more header.
    ///
    /// # Errors
    ///
    /// `500` when `value` is not a valid header value.
    pub fn header(mut self, name: HeaderName, value: &str) -> Result<Reply, ApiError> {
        self.headers.insert(name, header_value(value)?);
        Ok(self)
    }

    /// Sets the time that the server sends as `Last-Modified` and compares with
    /// `If-Modified-Since`.
    #[must_use]
    pub fn modified(mut self, at: Timestamp) -> Reply {
        self.modified = Some(at);
        self
    }
}

fn header_value(value: &str) -> Result<HeaderValue, ApiError> {
    HeaderValue::from_str(value).map_err(|e| ApiError::internal(e.to_string()))
}
