use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use http::Method;
use hubdouble_admin::Registrar;
use hubdouble_git::GitBuilder;
use hubdouble_model::{Config, ManualClock, Timestamp};
use hubdouble_server::{Registries, Server};
use hubdouble_store::{Deliveries, StoreBuilder};
use serde_json::Value;
use tokio::net::TcpListener;
use tokio::sync::oneshot;

use crate::client::{Answer, send};
use crate::coverage::Recorder;
use crate::fixture;
use crate::receiver::{Delivery, Receiver};

/// The admin token every test server uses.
pub const ADMIN_TOKEN: &str = "hubdouble-test-admin";

/// What a test server is built from. Start with [`TestServer::builder`].
pub struct Builder {
    frame: fn(&mut Registrar<'_>),
    registers: Vec<fn(&mut Registrar<'_>)>,
    seed: Vec<Value>,
    seed_file: Option<PathBuf>,
    env: HashMap<String, String>,
}

impl Builder {
    /// Replaces the admin frame (`hubdouble_admin::register`) with `f`, which then registers
    /// the admin frame itself, as `hubdouble_api::register_all` does for the whole server.
    #[must_use]
    pub fn frame(mut self, f: fn(&mut Registrar<'_>)) -> Builder {
        self.frame = f;
        self
    }

    /// Loads this seed file (YAML or JSON) as the server would, in place of
    /// [`Builder::fixture`] and [`Builder::entry`].
    #[must_use]
    pub fn seed_file(mut self, path: impl Into<PathBuf>) -> Builder {
        self.seed_file = Some(path.into());
        self
    }

    /// Adds a registration after the admin frame: the API crate under test or a stand-in from
    /// [`crate::standins`]. They run in the order added.
    #[must_use]
    pub fn register(mut self, f: fn(&mut Registrar<'_>)) -> Builder {
        self.registers.push(f);
        self
    }

    /// Appends the entries of [`fixture::seed`].
    #[must_use]
    pub fn fixture(mut self) -> Builder {
        self.seed.extend(fixture::seed());
        self
    }

    /// Appends one seed entry (`{"kind": ..., ...}`).
    #[must_use]
    pub fn entry(mut self, entry: Value) -> Builder {
        self.seed.push(entry);
        self
    }

    /// A variable the seed's `{{env "NAME"}}` templates read.
    #[must_use]
    pub fn env(mut self, name: &str, value: &str) -> Builder {
        self.env.insert(name.to_owned(), value.to_owned());
        self
    }

    /// Binds the receiver and the server on port 0, loads the seed and serves.
    ///
    /// # Panics
    ///
    /// When the server cannot start, the seed does not load, or both a seed file and entries are
    /// given.
    pub async fn start(self) -> TestServer {
        assert!(
            self.seed_file.is_none() || self.seed.is_empty(),
            "a test server takes a seed file or seed entries, not both"
        );
        let mut receiver = Receiver::start().await;
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap_or_else(|e| panic!("bind the test server: {e}"));
        let addr = listener
            .local_addr()
            .unwrap_or_else(|e| panic!("read the test server's address: {e}"));
        let dir =
            tempfile::TempDir::new().unwrap_or_else(|e| panic!("make the seed directory: {e}"));
        let seed = if let Some(file) = self.seed_file {
            file
        } else {
            let seed = dir.path().join("seed.json");
            let text =
                serde_json::to_vec(&self.seed).unwrap_or_else(|e| panic!("write the seed: {e}"));
            std::fs::write(&seed, text).unwrap_or_else(|e| panic!("write the seed: {e}"));
            seed
        };
        let base = format!("http://{addr}");
        let config = Config {
            addr,
            web_url: base.clone(),
            oidc_issuer: format!("{base}/_actions"),
            base_url: base.clone(),
            admin_token: ADMIN_TOKEN.to_owned(),
            seed: Some(seed),
            ..Config::default()
        };
        let clock = Arc::new(ManualClock::new(Timestamp::SEED));
        let mut env = self.env;
        env.insert(
            fixture::WEBHOOK_URL_ENV.to_owned(),
            receiver.url().to_owned(),
        );
        let lookup = |name: &str| env.get(name).cloned();
        let registries = registries(self.frame, &self.registers);
        let registered: HashSet<&'static str> = registries
            .rest
            .operations()
            .filter(|op| op.registered)
            .map(|op| op.operation_id)
            .collect();
        let server = Server::start(config, clock.clone(), &lookup, registries)
            .unwrap_or_else(|e| panic!("start the test server: {e}"));
        let recorder = Recorder::from_process_env();
        receiver.record_with(recorder.clone(), server.services().store.clone());
        let (stop, stopped) = oneshot::channel::<()>();
        let serving = server.clone();
        tokio::spawn(async move {
            serving
                .serve(listener, async {
                    let _ = stopped.await;
                })
                .await;
        });
        TestServer {
            addr,
            base,
            server,
            clock,
            receiver,
            http: reqwest::Client::new(),
            stop: Some(stop),
            registered: Arc::new(registered),
            recorder,
        }
    }
}

fn registries(frame: fn(&mut Registrar<'_>), extra: &[fn(&mut Registrar<'_>)]) -> Registries {
    let mut store = StoreBuilder::new();
    let git = GitBuilder::new(&mut store);
    hubdouble_auth::register_tables(&mut store);
    let mut r = Registries {
        store,
        rest: hubdouble_rest::Registry::new(),
        gql: hubdouble_gql::Registry::new(),
        git,
        admin: hubdouble_admin::Admin::new(),
    };
    for f in std::iter::once(&frame).chain(extra) {
        f(&mut Registrar {
            store: &mut r.store,
            rest: &mut r.rest,
            gql: &mut r.gql,
            git: &mut r.git,
            admin: &mut r.admin,
        });
    }
    r
}

/// One in-process server, its manual clock and its webhook receiver. Each test starts its own.
pub struct TestServer {
    pub addr: SocketAddr,
    /// `http://127.0.0.1:<port>`, the base and web URL of every answer.
    pub base: String,
    pub server: Server,
    pub clock: Arc<ManualClock>,
    pub receiver: Receiver,
    http: reqwest::Client,
    stop: Option<oneshot::Sender<()>>,
    pub(crate) registered: Arc<HashSet<&'static str>>,
    pub(crate) recorder: Recorder,
}

impl TestServer {
    /// Whether hubdouble implements `operation_id` on this server.
    #[must_use]
    pub fn registered(&self, operation_id: &str) -> bool {
        self.registered.contains(operation_id)
    }

    #[must_use]
    pub fn builder() -> Builder {
        Builder {
            frame: hubdouble_admin::register,
            registers: Vec::new(),
            seed: Vec::new(),
            seed_file: None,
            env: HashMap::new(),
        }
    }

    /// A request to the admin API (`path` under `/_hubdouble/`) with the admin token.
    ///
    /// # Panics
    ///
    /// When the request cannot be sent.
    pub async fn admin(&self, method: Method, path: &str, body: Option<&Value>) -> Answer {
        let url = format!("{}/_hubdouble/{}", self.base, path.trim_start_matches('/'));
        let mut request = self.http.request(method, url).bearer_auth(ADMIN_TOKEN);
        if let Some(body) = body {
            request = request.json(body);
        }
        send(request).await
    }

    /// Drains the webhook queue and returns every delivery received, in the order the deliveries
    /// were queued. Parallel workers deliver in any order, and GitHub promises no arrival order.
    ///
    /// # Panics
    ///
    /// When the drain fails, or a delivery is not in the delivery log.
    pub async fn deliveries_in_queue_order(&self) -> Vec<Delivery> {
        let drained = self.admin(Method::POST, "hooks/drain", None).await;
        assert_eq!(drained.status, http::StatusCode::NO_CONTENT);
        let mut received = self.receiver.received().await;
        let queued: HashMap<String, i64> = self
            .server
            .services()
            .store
            .view(|tx| {
                Ok(tx
                    .table::<Deliveries>()
                    .iter()
                    .map(|d| (d.guid.clone(), d.id))
                    .collect())
            })
            .unwrap_or_else(|e| panic!("read the delivery log: {e:?}"));
        received.sort_by_key(|d| queued[d.header("x-github-delivery")]);
        received
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
    }
}
