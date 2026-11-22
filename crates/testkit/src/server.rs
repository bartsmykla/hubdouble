use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use http::Method;
use hubdouble_admin::Registrar;
use hubdouble_git::GitBuilder;
use hubdouble_model::{Config, ManualClock, Timestamp};
use hubdouble_server::{Registries, Server};
use hubdouble_store::StoreBuilder;
use serde_json::Value;
use tokio::net::TcpListener;
use tokio::sync::oneshot;

use crate::client::{Answer, send};
use crate::fixture;
use crate::receiver::Receiver;

/// The admin token every test server uses.
pub const ADMIN_TOKEN: &str = "hubdouble-test-admin";

/// What a test server is built from. Start with [`TestServer::builder`].
pub struct Builder {
    registers: Vec<fn(&mut Registrar<'_>)>,
    seed: Vec<Value>,
    env: HashMap<String, String>,
}

impl Builder {
    /// Adds a registration after the phase 0 frame: the track under test or a stand-in from
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
    /// When the server cannot start or the seed does not load.
    pub async fn start(self) -> TestServer {
        let receiver = Receiver::start().await;
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap_or_else(|e| panic!("bind the test server: {e}"));
        let addr = listener
            .local_addr()
            .unwrap_or_else(|e| panic!("read the test server's address: {e}"));
        let dir =
            tempfile::TempDir::new().unwrap_or_else(|e| panic!("make the seed directory: {e}"));
        let seed = dir.path().join("seed.json");
        let text = serde_json::to_vec(&self.seed).unwrap_or_else(|e| panic!("write the seed: {e}"));
        std::fs::write(&seed, text).unwrap_or_else(|e| panic!("write the seed: {e}"));
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
        let registries = registries(&self.registers);
        let server = Server::start(config, clock.clone(), &lookup, registries)
            .unwrap_or_else(|e| panic!("start the test server: {e}"));
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
        }
    }
}

fn registries(extra: &[fn(&mut Registrar<'_>)]) -> Registries {
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
    for f in std::iter::once(&(hubdouble_admin::register as fn(&mut Registrar<'_>))).chain(extra) {
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
}

impl TestServer {
    #[must_use]
    pub fn builder() -> Builder {
        Builder {
            registers: Vec::new(),
            seed: Vec::new(),
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
}

impl Drop for TestServer {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
    }
}
