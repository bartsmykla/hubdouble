use serde::Serialize;

pub const DOCS: &str = "https://docs.github.com/rest";

/// One entry of a `422` answer's `errors[]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FieldError {
    pub resource: String,
    pub field: String,
    pub code: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// An error at the HTTP edge, written as GitHub's `{"message", "documentation_url", "status"}`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{status} {message}")]
pub struct ApiError {
    pub status: u16,
    pub message: String,
    pub documentation_url: String,
    pub errors: Vec<FieldError>,
    /// Extra response headers, such as `X-Accepted-GitHub-Permissions` or `X-GitHub-SSO`.
    pub headers: Vec<(&'static str, String)>,
}

impl ApiError {
    #[must_use]
    pub fn new(status: u16, message: impl Into<String>) -> ApiError {
        ApiError {
            status,
            message: message.into(),
            documentation_url: DOCS.to_owned(),
            errors: Vec::new(),
            headers: Vec::new(),
        }
    }

    #[must_use]
    pub fn bad_request(message: impl Into<String>) -> ApiError {
        ApiError::new(400, message)
    }

    #[must_use]
    pub fn unauthorized() -> ApiError {
        ApiError::new(401, "Bad credentials")
    }

    #[must_use]
    pub fn forbidden(message: impl Into<String>) -> ApiError {
        ApiError::new(403, message)
    }

    #[must_use]
    pub fn not_found() -> ApiError {
        ApiError::new(404, "Not Found")
    }

    #[must_use]
    pub fn conflict(message: impl Into<String>) -> ApiError {
        ApiError::new(409, message)
    }

    #[must_use]
    pub fn unprocessable(message: impl Into<String>) -> ApiError {
        ApiError::new(422, message)
    }

    #[must_use]
    pub fn internal(message: impl Into<String>) -> ApiError {
        ApiError::new(500, message)
    }

    /// The `501` of an operation, or a contract stub, that has no implementation yet.
    #[must_use]
    pub fn not_implemented(operation_id: &str) -> ApiError {
        ApiError::new(
            501,
            format!("hubdouble does not implement {operation_id} yet"),
        )
    }

    #[must_use]
    pub fn with_docs(mut self, url: impl Into<String>) -> ApiError {
        self.documentation_url = url.into();
        self
    }

    #[must_use]
    pub fn with_error(mut self, e: FieldError) -> ApiError {
        self.errors.push(e);
        self
    }

    #[must_use]
    pub fn with_header(mut self, name: &'static str, value: impl Into<String>) -> ApiError {
        self.headers.push((name, value.into()));
        self
    }
}
