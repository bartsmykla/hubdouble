use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http::{Method, Request};
use http_body_util::Full;
use hubdouble_git::{Git, GitBuilder};
use hubdouble_model::{ApiError, Clock, Config};
use hubdouble_rest::Params;
use hubdouble_store::{Snapshot, Store, StoreBuilder, TxMut};
use hyper::body::Incoming;
use parking_lot::Mutex;

use crate::faults::Faults;
use crate::log::Log;
use crate::{events, kinds, reads, seed};

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
    pub(crate) kinds: Vec<(&'static str, KindApply)>,
    pub(crate) routes: Vec<(Method, String, AdminHandler)>,
    pub(crate) snapshot: Mutex<Option<Arc<Snapshot>>>,
    pub(crate) log: Arc<Log>,
    pub(crate) faults: Faults,
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

    /// Answers a request under `/_hubdouble/`. Every route needs `Authorization: Bearer <admin
    /// token>`; without it the answer is `401`.
    pub async fn serve(
        &self,
        request: Request<Incoming>,
        services: Arc<Services>,
    ) -> AdminResponse {
        self.dispatch(request, services).await
    }

    /// Applies `HUBDOUBLE_SEED` with [`Store::update_quiet`] and takes the reset snapshot. A seed
    /// that fails changes nothing.
    ///
    /// # Errors
    ///
    /// An error naming the entry's position when an entry fails.
    pub fn load_seed(
        &self,
        services: &Services,
        env: &dyn Fn(&str) -> Option<String>,
    ) -> Result<(), ApiError> {
        let values = match &services.config.seed {
            Some(path) => {
                let text = std::fs::read_to_string(path).map_err(|e| {
                    ApiError::unprocessable(format!("seed {}: {e}", path.display()))
                })?;
                seed::parse(path, &text)?
            }
            None => Vec::new(),
        };
        let base = services.config.seed.as_deref().and_then(Path::parent);
        let entries = seed::prepare(values, &self.kinds, env, base)?;
        let ctx = seed::Env {
            git: &services.git,
            clock: &services.clock,
            lookup: env,
        };
        services
            .store
            .update_quiet(|tx| seed::apply(tx, entries, &ctx))?;
        *self.snapshot.lock() = Some(Arc::new(services.store.snapshot()));
        Ok(())
    }

    /// The request log hook: the server calls it once per API request.
    pub fn record(&self, entry: &LogEntry<'_>) {
        self.log.record(entry);
    }

    /// The delivery runtime's observer: it puts each event and each logged delivery in the
    /// request log.
    #[must_use]
    pub fn observer(&self) -> hubdouble_hooks::Observer {
        let log = self.log.clone();
        Arc::new(move |notice| log.note(notice))
    }

    /// The faults hook: the server calls it before it runs a request. A fault with a `count`
    /// answers that many requests.
    #[must_use]
    pub fn fault(&self, query: &FaultQuery<'_>) -> Option<Fault> {
        self.faults.take(query)
    }
}

/// Registers P0-F's own kinds (users, orgs, repositories, Apps, installations, tokens) and admin
/// routes; `hubdouble-api::register_all` calls it before every track.
pub fn register(r: &mut Registrar<'_>) {
    r.admin.kind("user", kinds::user_kind);
    r.admin.kind("org", kinds::org_kind);
    r.admin.kind("repository", kinds::repository_kind);
    r.admin.kind("app", kinds::app_kind);
    r.admin.kind("installation", kinds::installation_kind);
    r.admin.kind("token", kinds::token_kind);
    r.admin
        .route(Method::GET, "/user/{login}", reads::user_route());
    r.admin
        .route(Method::GET, "/app/{slug}", reads::app_route());
    r.admin.route(Method::POST, "/events", events::route());
}

/// Handed to each track crate's `pub fn register(r: &mut Registrar)`.
pub struct Registrar<'a> {
    pub store: &'a mut StoreBuilder,
    pub rest: &'a mut hubdouble_rest::Registry,
    pub gql: &'a mut hubdouble_gql::Registry,
    pub git: &'a mut GitBuilder,
    pub admin: &'a mut Admin,
}
