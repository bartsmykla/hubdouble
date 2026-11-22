use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::sync::Arc;

use hubdouble_auth::{Actor, Target as AuthTarget};
use hubdouble_git::Git;
use hubdouble_model::{ApiError, Clock, Repo, Requirement};
use hubdouble_render::Urls;
use hubdouble_store::{Store, Tx, TxMut};
use serde::de::DeserializeOwned;
use serde::{Serialize, Serializer};

pub use crate::connection::{Conn, Edge, PageInfo, connection};
use crate::schema::Schema;

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
    InvalidPagination,
    MaxNodeLimitExceeded,
    InvalidFieldInput,
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
            ErrorType::InvalidPagination => "INVALID_PAGINATION",
            ErrorType::MaxNodeLimitExceeded => "MAX_NODE_LIMIT_EXCEEDED",
            ErrorType::InvalidFieldInput | ErrorType::Validation => "VALIDATION",
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
    pub registry: &'a Registry,
}

impl Ctx<'_> {
    /// The repository, org or node behind `t`, after the actor's grant check.
    /// A node ID resolves through the loader [`Registry::node`] registered for its prefix; a
    /// legacy ID through the loader of its type name.
    ///
    /// # Errors
    ///
    /// `NOT_FOUND` for an object the actor cannot see or that does not exist.
    pub fn load(&self, t: Target<'_>) -> Result<Resolved, GqlError> {
        crate::load::load(self, t)
    }
}

/// A mutation resolver's view: its own `update`, next to the same `Git`.
pub struct MutCtx<'a> {
    pub tx: TxMut<'a>,
    pub git: &'a Git,
    pub actor: &'a Actor,
    pub clock: &'a dyn Clock,
    pub urls: &'a Urls,
    pub registry: &'a Registry,
}

impl MutCtx<'_> {
    /// A query view of the mutation's transaction, which sees the mutation's own writes.
    #[must_use]
    pub fn ctx(&self) -> Ctx<'_> {
        Ctx {
            tx: &self.tx,
            git: self.git,
            actor: self.actor,
            clock: self.clock,
            urls: self.urls,
            registry: self.registry,
        }
    }

    /// As [`Ctx::load`], inside the mutation's transaction.
    ///
    /// # Errors
    ///
    /// `NOT_FOUND` for an object the actor cannot see or that does not exist.
    pub fn load(&self, t: Target<'_>) -> Result<Resolved, GqlError> {
        self.ctx().load(t)
    }

    /// Checks `req` against the actor's grant on `repo` through the authorization choke point,
    /// before the mutation changes anything.
    ///
    /// # Errors
    ///
    /// `NOT_FOUND` for a repository the actor cannot see, `FORBIDDEN` for a missing permission.
    pub fn authorize(&self, req: &Requirement, repo: &Arc<Repo>) -> Result<(), GqlError> {
        hubdouble_auth::authorize(&self.tx, self.actor, req, AuthTarget::Repo(repo))
            .map_err(GqlError::from)
    }
}

impl From<ApiError> for GqlError {
    fn from(e: ApiError) -> GqlError {
        let kind = match e.status {
            404 => ErrorType::NotFound,
            401 | 403 => ErrorType::Forbidden,
            422 | 409 => ErrorType::Unprocessable,
            501 => ErrorType::NotImplemented,
            _ => ErrorType::Internal,
        };
        GqlError::new(kind, e.message)
    }
}

pub type Resolver = fn(&Ctx<'_>, &Obj, &Args) -> Result<Resolved, GqlError>;
pub type MutationResolver = fn(&mut MutCtx<'_>, &Args) -> Result<Resolved, GqlError>;
pub type NodeLoader = fn(&Ctx<'_>, i64) -> Result<Resolved, GqlError>;

pub(crate) struct ObjectType {
    pub(crate) type_id: TypeId,
    pub(crate) rust_name: &'static str,
    pub(crate) serialize: fn(&(dyn Any + Send + Sync)) -> Option<serde_json::Value>,
}

pub(crate) struct NodeType {
    pub(crate) prefix: &'static str,
    pub(crate) type_name: &'static str,
    pub(crate) load: NodeLoader,
}

/// The resolvers of every GraphQL type, field, mutation and node prefix; frozen at start.
pub struct Registry {
    pub(crate) schema: Arc<Schema>,
    pub(crate) objects: HashMap<&'static str, ObjectType>,
    pub(crate) fields: HashMap<String, HashMap<String, Resolver>>,
    pub(crate) mutations: HashMap<String, MutationResolver>,
    pub(crate) nodes: Vec<NodeType>,
    pub(crate) root: Arc<dyn Any + Send + Sync>,
}

impl Default for Registry {
    fn default() -> Registry {
        Registry::new()
    }
}

fn serialize_as<T: Serialize + 'static>(v: &(dyn Any + Send + Sync)) -> Option<serde_json::Value> {
    serde_json::to_value(v.downcast_ref::<T>()?).ok()
}

impl Registry {
    /// A registry on GitHub's pinned schema, parsed here.
    ///
    /// # Panics
    ///
    /// Panics when the embedded schema does not load, which a test of this crate rules out.
    #[must_use]
    pub fn new() -> Registry {
        match Schema::load() {
            Ok(schema) => Registry::with_schema(Arc::new(schema)),
            Err(e) => panic!("the embedded GraphQL schema does not load: {e}"),
        }
    }

    /// A registry on an already parsed schema, so tests can share one parse.
    #[must_use]
    pub fn with_schema(schema: Arc<Schema>) -> Registry {
        Registry {
            schema,
            objects: HashMap::new(),
            fields: HashMap::new(),
            mutations: HashMap::new(),
            nodes: Vec::new(),
            root: Arc::new(()),
        }
    }

    #[must_use]
    pub fn schema(&self) -> &Arc<Schema> {
        &self.schema
    }

    /// Declares the Rust value behind `type_name`; only the type's owner calls it.
    ///
    /// # Panics
    ///
    /// Panics when `type_name` is not an object type of the schema or is already declared.
    pub fn object<T: Serialize + Send + Sync + 'static>(&mut self, type_name: &'static str) {
        self.declare::<T>(type_name, serialize_as::<T>);
    }

    /// As [`Registry::object`], for a value without a `serde` form: every field the documents
    /// select needs a resolver, and a field without one answers `NOT_IMPLEMENTED`.
    ///
    /// # Panics
    ///
    /// As [`Registry::object`].
    pub fn object_resolved<T: Send + Sync + 'static>(&mut self, type_name: &'static str) {
        self.declare::<T>(type_name, |_| None);
    }

    fn declare<T: 'static>(
        &mut self,
        type_name: &'static str,
        serialize: fn(&(dyn Any + Send + Sync)) -> Option<serde_json::Value>,
    ) {
        assert!(
            self.schema.is_object(type_name),
            "{type_name} is not an object type of the schema"
        );
        let previous = self.objects.insert(
            type_name,
            ObjectType {
                type_id: TypeId::of::<T>(),
                rust_name: std::any::type_name::<T>(),
                serialize,
            },
        );
        assert!(previous.is_none(), "{type_name} is declared twice");
    }

    /// # Panics
    ///
    /// Panics on a second registration of the same field, and on a field the schema does not
    /// define on the object type. `Query.node` and `Query.nodes` are this crate's.
    pub fn field(&mut self, type_name: &str, field_name: &str, r: Resolver) {
        assert!(
            self.schema.is_object(type_name)
                && self.schema.inner.type_field(type_name, field_name).is_ok(),
            "{type_name}.{field_name} is not a field of an object type of the schema"
        );
        let builtin = type_name == "Query" && matches!(field_name, "node" | "nodes");
        let previous = self
            .fields
            .entry(type_name.to_owned())
            .or_default()
            .insert(field_name.to_owned(), r);
        assert!(
            previous.is_none() && !builtin,
            "{type_name}.{field_name} is registered twice"
        );
    }

    /// # Panics
    ///
    /// Panics on a second registration, and on a name that is not a mutation of the schema.
    pub fn mutation(&mut self, name: &str, r: MutationResolver) {
        assert!(
            self.schema.inner.type_field("Mutation", name).is_ok(),
            "{name} is not a mutation of the schema"
        );
        let previous = self.mutations.insert(name.to_owned(), r);
        assert!(previous.is_none(), "mutation {name} is registered twice");
    }

    /// Loads the objects whose node IDs carry `prefix`, for `node` and `nodes`.
    ///
    /// # Panics
    ///
    /// Panics when `prefix` or `type_name` already has a loader.
    pub fn node(&mut self, prefix: &'static str, type_name: &'static str, load: NodeLoader) {
        assert!(
            !self
                .nodes
                .iter()
                .any(|n| n.prefix == prefix || n.type_name == type_name),
            "node prefix {prefix} or type {type_name} is registered twice"
        );
        self.nodes.push(NodeType {
            prefix,
            type_name,
            load,
        });
    }

    pub(crate) fn resolver(&self, type_name: &str, field_name: &str) -> Option<Resolver> {
        self.fields.get(type_name)?.get(field_name).copied()
    }
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extensions: Option<serde_json::Value>,
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
#[expect(
    clippy::needless_pass_by_value,
    reason = "the frozen contract takes the request by value"
)]
pub fn execute(
    registry: &Registry,
    store: &Store,
    git: &Git,
    actor: &Actor,
    request: Request<'_>,
) -> Result<Response, ApiError> {
    crate::exec::execute(registry, store, git, actor, &request)
}
