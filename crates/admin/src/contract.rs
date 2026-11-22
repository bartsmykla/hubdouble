use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http::{Method, Request, StatusCode};
use http_body_util::Full;
use hubdouble_git::{Git, GitBuilder};
use hubdouble_model::{ApiError, Clock, Config};
use hubdouble_rest::Params;
use hubdouble_store::{Store, StoreBuilder, TxMut};
use hyper::body::Incoming;

/// What a seed kind applies with: the transaction, git, the clock and the environment lookup.
pub struct SeedCtx<'a> {
    pub tx: TxMut<'a>,
    pub git: &'a Git,
    pub clock: &'a dyn Clock,
    pub env: &'a dyn Fn(&str) -> Option<String>,
}

/// Applies one seed object of a kind; also serves `POST /_hubdouble/{kind}`.
pub type KindApply = fn(&mut SeedCtx<'_>, serde_json::Value) -> Result<(), ApiError>;

/// Built by the server after the registries freeze.
pub struct Services {
    pub store: Store,
    pub git: Git,
    pub hooks: hubdouble_hooks::Handle,
    pub clock: Arc<dyn Clock>,
    pub config: Arc<Config>,
}

pub struct AdminRequest {
    pub request: Request<Incoming>,
    pub params: Params,
    pub services: Arc<Services>,
}

pub type AdminResponse = hyper::Response<Full<Bytes>>;
pub type AdminHandler =
    Arc<dyn Fn(AdminRequest) -> Pin<Box<dyn Future<Output = AdminResponse> + Send>> + Send + Sync>;

/// One line of the request log the server writes for every API request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LogEntry<'a> {
    pub method: &'a str,
    pub path: &'a str,
    pub operation_id: &'a str,
    pub actor: Option<&'a str>,
    pub status: u16,
}

/// What the server asks the fault list about before it runs a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FaultQuery<'a> {
    pub method: &'a str,
    pub path: &'a str,
    /// The REST operation ID, or the GraphQL operation name.
    pub operation: &'a str,
}

/// An injected answer that replaces the real one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fault {
    pub status: u16,
    pub body: Bytes,
    pub headers: Vec<(String, String)>,
    pub delay: Duration,
}

/// The admin API: seed kinds in registration order, extra admin routes, faults and the request log.
#[derive(Default)]
pub struct Admin {
    kinds: Vec<(&'static str, KindApply)>,
    routes: Vec<(Method, String, AdminHandler)>,
}

impl Admin {
    #[must_use]
    pub fn new() -> Admin {
        Admin::default()
    }

    /// A seed kind and `POST /_hubdouble/{name}`; kinds apply in registration order.
    pub fn kind(&mut self, name: &'static str, apply: KindApply) {
        self.kinds.push((name, apply));
    }

    /// An extra admin route under `/_hubdouble/`.
    pub fn route(&mut self, method: Method, path: &str, h: AdminHandler) {
        self.routes.push((method, path.to_owned(), h));
    }

    /// The registered kind names, in application order.
    pub fn kind_names(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.kinds.iter().map(|(n, _)| *n)
    }

    /// Answers a request under `/_hubdouble/`.
    #[expect(
        clippy::unused_async,
        clippy::unused_async_trait_impl,
        reason = "the contract is async and the P0-F body awaits"
    )]
    pub async fn serve(
        &self,
        request: Request<Incoming>,
        services: Arc<Services>,
    ) -> AdminResponse {
        let _ = (request, services, &self.routes);
        let body = r#"{"message":"hubdouble does not implement the admin API yet","documentation_url":"https://docs.github.com/rest","status":"501"}"#;
        let mut response = hyper::Response::new(Full::new(Bytes::from_static(body.as_bytes())));
        *response.status_mut() = StatusCode::NOT_IMPLEMENTED;
        response
    }

    /// Applies `HUBDOUBLE_SEED` with [`Store::update_quiet`] and takes the reset snapshot.
    ///
    /// # Errors
    ///
    /// An error naming the entry's position when an entry fails.
    pub fn load_seed(
        &self,
        services: &Services,
        env: &dyn Fn(&str) -> Option<String>,
    ) -> Result<(), ApiError> {
        let _ = (services, env);
        Err(ApiError::not_implemented("admin/seed"))
    }

    /// The request log hook: the server calls it once per API request.
    pub fn record(&self, entry: &LogEntry<'_>) {
        let _ = entry;
    }

    /// The faults hook: the server calls it before it runs a request.
    #[must_use]
    pub fn fault(&self, query: &FaultQuery<'_>) -> Option<Fault> {
        let _ = query;
        None
    }
}

/// Registers P0-F's own kinds (users, orgs, repositories, Apps, installations, tokens) and admin
/// routes; `hubdouble-api::register_all` calls it before every track.
pub fn register(r: &mut Registrar<'_>) {
    let _ = r;
}

/// Handed to each track crate's `pub fn register(r: &mut Registrar)`.
pub struct Registrar<'a> {
    pub store: &'a mut StoreBuilder,
    pub rest: &'a mut hubdouble_rest::Registry,
    pub gql: &'a mut hubdouble_gql::Registry,
    pub git: &'a mut GitBuilder,
    pub admin: &'a mut Admin,
}
