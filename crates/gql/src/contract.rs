use std::any::Any;
use std::sync::Arc;

use hubdouble_auth::Actor;
use hubdouble_git::Git;
use hubdouble_model::{ApiError, Clock};
use hubdouble_render::Urls;
use hubdouble_store::{Store, Tx, TxMut};
use serde::de::DeserializeOwned;
use serde::{Serialize, Serializer};

/// GitHub's GraphQL error `type`s, plus hubdouble's own for an unimplemented field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorType {
    NotFound,
    Forbidden,
    Unprocessable,
    Internal,
    NotImplemented,
    /// A connection without `first` or `last`.
    MissingPaginationBoundaries,
    /// A connection with `first` or `last` above 100.
    ExcessivePagination,
    /// A document or variable error; GitHub sends these without a `type`.
    Validation,
}

impl ErrorType {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            ErrorType::NotFound => "NOT_FOUND",
            ErrorType::Forbidden => "FORBIDDEN",
            ErrorType::Unprocessable => "UNPROCESSABLE",
            ErrorType::Internal => "INTERNAL",
            ErrorType::NotImplemented => "HUBDOUBLE_NOT_IMPLEMENTED",
            ErrorType::MissingPaginationBoundaries => "MISSING_PAGINATION_BOUNDARIES",
            ErrorType::ExcessivePagination => "EXCESSIVE_PAGINATION",
            ErrorType::Validation => "VALIDATION",
        }
    }

    #[must_use]
    pub fn is_validation(&self) -> bool {
        *self == ErrorType::Validation
    }
}

impl Serialize for ErrorType {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

/// A field error; the executor adds `path` and `locations`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct GqlError {
    pub kind: ErrorType,
    pub message: String,
}

impl GqlError {
    #[must_use]
    pub fn new(kind: ErrorType, message: impl Into<String>) -> GqlError {
        GqlError {
            kind,
            message: message.into(),
        }
    }
}

/// What a resolver returns.
#[derive(Clone)]
pub enum Resolved {
    Null,
    /// Scalars and enums.
    Leaf(serde_json::Value),
    /// An object of `type_name`; `value` holds the Rust value [`Registry::object`] registered for it.
    Object {
        type_name: &'static str,
        value: Arc<dyn Any + Send + Sync>,
    },
    List(Vec<Resolved>),
}

/// The parent object a field resolves on.
#[derive(Clone)]
pub struct Obj {
    pub type_name: &'static str,
    pub value: Arc<dyn Any + Send + Sync>,
}

impl Obj {
    /// The parent's Rust value.
    ///
    /// # Errors
    ///
    /// An `INTERNAL` error when the parent is not a `T`.
    pub fn get<T: Any>(&self) -> Result<&T, GqlError> {
        (*self.value).downcast_ref::<T>().ok_or_else(|| {
            GqlError::new(
                ErrorType::Internal,
                format!("{} is not a {}", self.type_name, std::any::type_name::<T>()),
            )
        })
    }
}

/// A field's arguments, with variables substituted and defaults applied.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Args(pub serde_json::Map<String, serde_json::Value>);

impl Args {
    /// An argument, or `None` when it is absent or `null`.
    ///
    /// # Errors
    ///
    /// An `UNPROCESSABLE` error when the value is not a `T`.
    pub fn get<T: DeserializeOwned>(&self, name: &str) -> Result<Option<T>, GqlError> {
        match self.0.get(name) {
            None | Some(serde_json::Value::Null) => Ok(None),
            Some(v) => serde_json::from_value(v.clone())
                .map(Some)
                .map_err(|e| GqlError::new(ErrorType::Unprocessable, e.to_string())),
        }
    }
}

/// What [`Ctx::load`] fetches with the actor's per-object grant check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target<'t> {
    Repo { owner: &'t str, name: &'t str },
    RepoId(i64),
    Org(&'t str),
    Node(&'t str),
}

/// A query resolver's view: one `view` of the store.
pub struct Ctx<'a> {
    pub tx: &'a Tx,
    pub git: &'a Git,
    pub actor: &'a Actor,
    pub clock: &'a dyn Clock,
    pub urls: &'a Urls,
}

impl Ctx<'_> {
    /// The repository, org or node behind `t`, after the actor's grant check.
    ///
    /// # Errors
    ///
    /// `NOT_FOUND` for an object the actor cannot see or that does not exist.
    pub fn load(&self, t: Target<'_>) -> Result<Resolved, GqlError> {
        let _ = t;
        Err(GqlError::new(ErrorType::NotImplemented, "gql/load"))
    }
}

/// A mutation resolver's view: its own `update`, next to the same `Git`.
pub struct MutCtx<'a> {
    pub tx: TxMut<'a>,
    pub git: &'a Git,
    pub actor: &'a Actor,
    pub clock: &'a dyn Clock,
    pub urls: &'a Urls,
}

impl MutCtx<'_> {
    /// As [`Ctx::load`], inside the mutation's transaction.
    ///
    /// # Errors
    ///
    /// `NOT_FOUND` for an object the actor cannot see or that does not exist.
    pub fn load(&self, t: Target<'_>) -> Result<Resolved, GqlError> {
        let _ = t;
        Err(GqlError::new(ErrorType::NotImplemented, "gql/load"))
    }
}

pub type Resolver = fn(&Ctx<'_>, &Obj, &Args) -> Result<Resolved, GqlError>;
pub type MutationResolver = fn(&mut MutCtx<'_>, &Args) -> Result<Resolved, GqlError>;
pub type NodeLoader = fn(&Ctx<'_>, i64) -> Result<Resolved, GqlError>;

/// The resolvers of every GraphQL type, field, mutation and node prefix; frozen at start.
#[derive(Default)]
pub struct Registry {
    _frozen: (),
}

impl Registry {
    #[must_use]
    pub fn new() -> Registry {
        Registry::default()
    }

    /// Declares the Rust value behind `type_name`; only the type's owner calls it.
    pub fn object<T: Serialize + Send + Sync + 'static>(&mut self, type_name: &'static str) {
        let _ = type_name;
        todo!("P0-B: gql::Registry::object")
    }

    /// # Panics
    ///
    /// Panics on a second registration of the same field.
    pub fn field(&mut self, type_name: &str, field_name: &str, r: Resolver) {
        let _ = (type_name, field_name, r);
        todo!("P0-B: gql::Registry::field")
    }

    pub fn mutation(&mut self, name: &str, r: MutationResolver) {
        let _ = (name, r);
        todo!("P0-B: gql::Registry::mutation")
    }

    /// Loads the objects whose node IDs carry `prefix`, for `node` and `nodes`.
    pub fn node(&mut self, prefix: &'static str, type_name: &'static str, load: NodeLoader) {
        let _ = (prefix, type_name, load);
        todo!("P0-B: gql::Registry::node")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PageInfo {
    pub has_next_page: bool,
    pub has_previous_page: bool,
    pub start_cursor: Option<String>,
    pub end_cursor: Option<String>,
}

/// An edge of a [`Conn`].
#[derive(Clone)]
pub struct Edge {
    pub cursor: String,
    pub node: Resolved,
}

/// The value behind every connection type P0-B owns.
#[derive(Clone)]
pub struct Conn {
    pub type_name: &'static str,
    pub edges: Vec<Edge>,
    pub page_info: PageInfo,
    pub total_count: usize,
}

/// Slices `items` by `first`, `last`, `after` and `before` into a [`Conn`] named `type_name`
/// (`PullRequestConnection`), an empty page included.
///
/// # Errors
///
/// GitHub's error for a connection without `first` or `last`, or with more than 100.
pub fn connection<T>(
    type_name: &'static str,
    items: &[T],
    args: &Args,
    cursor: impl Fn(&T) -> String,
    node: impl Fn(&T) -> Resolved,
) -> Result<Resolved, GqlError> {
    let _ = (type_name, items, args, cursor, node);
    Err(GqlError::new(ErrorType::NotImplemented, "gql/connection"))
}

/// One GraphQL request, as the server decoded it.
pub struct Request<'a> {
    pub query: &'a str,
    pub operation_name: Option<&'a str>,
    pub variables: &'a serde_json::Map<String, serde_json::Value>,
    pub clock: &'a dyn Clock,
    pub urls: &'a Urls,
}

/// One error of a [`Response`], with GitHub's `type`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ResponseError {
    #[serde(rename = "type", skip_serializing_if = "ErrorType::is_validation")]
    pub kind: ErrorType,
    pub message: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub path: Vec<serde_json::Value>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub locations: Vec<serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Response {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<ResponseError>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extensions: Option<serde_json::Map<String, serde_json::Value>>,
}

/// Runs a query in one `view`, and each mutation in its own `update`, in order.
///
/// # Errors
///
/// An [`ApiError`] only for a request the executor cannot run at all.
pub fn execute(
    registry: &Registry,
    store: &Store,
    git: &Git,
    actor: &Actor,
    request: Request<'_>,
) -> Result<Response, ApiError> {
    let _ = (registry, store, git, actor, request);
    Err(ApiError::not_implemented("graphql"))
}
