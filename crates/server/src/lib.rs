//! The HTTP edge: environment parsing, the listener, the request pipeline, the error writer,
//! conditional requests, faults, `/_blobs` and graceful shutdown.

mod blobs;
mod config;
mod pipeline;

#[cfg(test)]
mod tests;

use std::convert::Infallible;
use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;

use hubdouble_admin::{Admin, Registrar, Services};
use hubdouble_git::GitBuilder;
use hubdouble_model::{ApiError, Clock, Config};
use hubdouble_store::StoreBuilder;
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto;
use hyper_util::server::graceful::GracefulShutdown;
use tokio::net::TcpListener;

pub use config::{ConfigError, config_from_env};

#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    #[error("bind {addr}: {source}")]
    Bind {
        addr: SocketAddr,
        source: std::io::Error,
    },
    #[error("start the webhook workers: {0}")]
    Hooks(#[from] hubdouble_hooks::HooksError),
    #[error("load the seed: {0}")]
    Seed(ApiError),
}

/// Every registry, still open: `register` (normally `hubdouble_api::register_all`) fills them.
pub struct Registries {
    pub store: StoreBuilder,
    pub rest: hubdouble_rest::Registry,
    pub gql: hubdouble_gql::Registry,
    pub git: GitBuilder,
    pub admin: Admin,
}

impl Registries {
    #[must_use]
    pub fn new(register: fn(&mut Registrar<'_>)) -> Registries {
        let mut store = StoreBuilder::new();
        let git = GitBuilder::new(&mut store);
        hubdouble_auth::register_tables(&mut store);
        let mut r = Registries {
            store,
            rest: hubdouble_rest::Registry::new(),
            gql: hubdouble_gql::Registry::new(),
            git,
            admin: Admin::new(),
        };
        register(&mut Registrar {
            store: &mut r.store,
            rest: &mut r.rest,
            gql: &mut r.gql,
            git: &mut r.git,
            admin: &mut r.admin,
        });
        r
    }
}

/// A started server: the registries are frozen and the webhook workers run.
#[derive(Clone)]
pub struct Server {
    inner: Arc<pipeline::Inner>,
}

impl Server {
    /// Freezes the registries, starts the webhook workers and applies `config.seed` when set.
    /// Call it inside a tokio runtime.
    ///
    /// # Errors
    ///
    /// [`ServerError`] when the workers cannot start or the seed does not apply.
    pub fn start(
        config: Config,
        clock: Arc<dyn Clock>,
        env: &dyn Fn(&str) -> Option<String>,
        registries: Registries,
    ) -> Result<Server, ServerError> {
        let Registries {
            store,
            rest,
            gql,
            git,
            admin,
        } = registries;
        let config = Arc::new(config);
        let store = store.build_with(config.clone());
        let hooks = hubdouble_hooks::Handle::start(store.clone(), clock.clone(), config.clone())?;
        let services = Arc::new(Services {
            store,
            git: git.build(),
            hooks,
            clock,
            config,
        });
        if services.config.seed.is_some() {
            admin.load_seed(&services, env).map_err(ServerError::Seed)?;
        }
        Ok(Server {
            inner: Arc::new(pipeline::Inner::new(services, rest, gql, admin)),
        })
    }

    #[must_use]
    pub fn services(&self) -> &Arc<Services> {
        &self.inner.services
    }

    /// Binds `config.addr`; bind before [`Server::serve`] so the port is known and taken early.
    ///
    /// # Errors
    ///
    /// [`ServerError::Bind`] when the address is not free.
    pub async fn bind(&self) -> Result<TcpListener, ServerError> {
        let addr = self.inner.services.config.addr;
        TcpListener::bind(addr)
            .await
            .map_err(|source| ServerError::Bind { addr, source })
    }

    /// Serves HTTP/1.1 and h2c on `listener` until `shutdown` completes, then lets open
    /// connections finish for at most `config.shutdown_grace` and stops the webhook workers.
    pub async fn serve(&self, listener: TcpListener, shutdown: impl Future<Output = ()>) {
        let graceful = GracefulShutdown::new();
        let builder = auto::Builder::new(TokioExecutor::new());
        tokio::pin!(shutdown);
        loop {
            tokio::select! {
                accepted = listener.accept() => {
                    let Ok((stream, _)) = accepted else { continue };
                    let inner = self.inner.clone();
                    let service = hyper::service::service_fn(move |req| {
                        let inner = inner.clone();
                        async move { Ok::<_, Infallible>(inner.handle(req).await) }
                    });
                    let conn = graceful.watch(builder.serve_connection(TokioIo::new(stream), service).into_owned());
                    tokio::spawn(async move {
                        let _ = conn.await;
                    });
                }
                () = &mut shutdown => break,
            }
        }
        drop(listener);
        tokio::select! {
            () = graceful.shutdown() => {}
            () = tokio::time::sleep(self.inner.services.config.shutdown_grace) => {}
        }
        self.inner.services.hooks.shutdown().await;
    }
}
