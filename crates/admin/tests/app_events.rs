use std::convert::Infallible;
use std::sync::Arc;

use bytes::Bytes;
use http::{Method, Request, Response, StatusCode, Uri, header::AUTHORIZATION};
use http_body_util::Full;
use hubdouble_admin::{Admin, Registrar, Services};
use hubdouble_git::GitBuilder;
use hubdouble_hooks::{ConnectFuture, Connector, Handle, Io, Options};
use hubdouble_model::{Config, ManualClock, Timestamp};
use hubdouble_store::{Deliveries, StoreBuilder};
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use serde_json::{Value, json};

const APP_ID: i64 = 20;
const KEY: &str = "-----BEGIN PUBLIC KEY-----\nMFkw\n-----END PUBLIC KEY-----\n";

struct Receiver;

impl Connector for Receiver {
    fn connect(&self, _: &Uri) -> ConnectFuture {
        Box::pin(async {
            let (client, server) = tokio::io::duplex(1 << 16);
            let service = service_fn(|_| async {
                Ok::<_, Infallible>(Response::new(Full::new(Bytes::new())))
            });
            tokio::spawn(
                hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(server), service),
            );
            Ok(Box::new(client) as Box<dyn Io>)
        })
    }
}

struct Fixture {
    admin: Arc<Admin>,
    services: Arc<Services>,
    _dir: tempfile::TempDir,
}

fn app() -> Value {
    json!({
        "id": APP_ID, "slug": "demo", "owner": "alice", "public_key": KEY,
        "webhook": {"url": "http://example.com/hook"},
    })
}

fn installation() -> Value {
    json!({"id": 30, "app": "demo", "account": "acme", "repositories": ["web"]})
}

#[expect(
    clippy::unwrap_used,
    reason = "the admin tests require a valid seed fixture"
)]
async fn fixture(seed_apps: bool) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("seed.json");
    let mut seed = vec![
        json!({"kind": "user", "id": 1, "login": "alice"}),
        json!({"kind": "org", "id": 2, "login": "acme"}),
        json!({"kind": "repository", "id": 3, "owner": "acme", "name": "web"}),
    ];
    if seed_apps {
        let mut app = app();
        app["kind"] = json!("app");
        let mut installation = installation();
        installation["kind"] = json!("installation");
        seed.extend([app, installation]);
    }
    std::fs::write(&path, serde_json::to_vec(&seed).unwrap()).unwrap();
    let config = Arc::new(Config {
        seed: Some(path),
        ..Config::default()
    });
    let clock = Arc::new(ManualClock::new(Timestamp::SEED));
    let mut builder = StoreBuilder::new();
    let mut git = GitBuilder::new(&mut builder);
    let mut admin = Admin::new();
    hubdouble_admin::register(&mut Registrar {
        store: &mut builder,
        rest: &mut hubdouble_rest::Registry::new(),
        gql: &mut hubdouble_gql::Registry::new(),
        git: &mut git,
        admin: &mut admin,
    });
    git.clock(clock.clone());
    let git = git.build();
    let store = builder.build_with(config.clone());
    let hooks = Handle::start_with(
        store.clone(),
        clock.clone(),
        Options {
            connector: Arc::new(Receiver),
            ..Options::default()
        },
    )
    .unwrap();
    let services = Arc::new(Services {
        store,
        git,
        hooks,
        clock,
        config,
    });
    admin.load_seed(&services, &|_| None).unwrap();
    services.hooks.drain().await.unwrap();
    Fixture {
        admin: Arc::new(admin),
        services,
        _dir: dir,
    }
}

#[expect(
    clippy::unwrap_used,
    reason = "a test POST must reach the admin handler"
)]
async fn post(fixture: &Fixture, kind: &str, body: &Value) -> StatusCode {
    let (client, server) = tokio::io::duplex(1 << 16);
    let admin = fixture.admin.clone();
    let services = fixture.services.clone();
    let service = service_fn(move |request| {
        let admin = admin.clone();
        let services = services.clone();
        async move { Ok::<_, Infallible>(admin.serve(request, services).await) }
    });
    tokio::spawn(
        hyper::server::conn::http1::Builder::new().serve_connection(TokioIo::new(server), service),
    );
    let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(client))
        .await
        .unwrap();
    tokio::spawn(connection);
    let request = Request::builder()
        .method(Method::POST)
        .uri(format!("/_hubdouble/{kind}"))
        .header(
            AUTHORIZATION,
            format!("Bearer {}", fixture.services.config.admin_token),
        )
        .body(Full::new(Bytes::from(serde_json::to_vec(body).unwrap())))
        .unwrap();
    let response = sender.send_request(request).await.unwrap();
    fixture.services.hooks.drain().await.unwrap();
    response.status()
}

#[tokio::test(start_paused = true)]
async fn admin_app_create_sends_ping() {
    let fixture = fixture(false).await;
    assert_eq!(post(&fixture, "app", &app()).await, StatusCode::CREATED);
    fixture
        .services
        .store
        .view(|tx| {
            let delivery = tx.table::<Deliveries>().children(APP_ID).next().unwrap();
            assert_eq!(delivery.event, "ping");
            assert_eq!(delivery.status_code, 200);
            assert_eq!(delivery.request.payload["hook"]["app_id"], APP_ID);
            Ok(())
        })
        .unwrap();
}

#[tokio::test(start_paused = true)]
async fn admin_installation_create_sends_its_repositories() {
    let fixture = fixture(false).await;
    assert_eq!(post(&fixture, "app", &app()).await, StatusCode::CREATED);
    let mut installation = installation();
    installation["id"] = json!(100);
    assert_eq!(
        post(&fixture, "installation", &installation).await,
        StatusCode::CREATED
    );
    fixture
        .services
        .store
        .view(|tx| {
            let delivery = tx
                .table::<Deliveries>()
                .children(APP_ID)
                .next_back()
                .unwrap();
            assert_eq!(delivery.event, "installation");
            assert_eq!(delivery.installation_id, Some(100));
            assert_eq!(delivery.status_code, 200);
            let payload = &delivery.request.payload;
            assert_eq!(payload["action"], "created");
            assert_eq!(payload["installation"]["id"], 100);
            assert_eq!(payload["repositories"][0]["full_name"], "acme/web");
            Ok(())
        })
        .unwrap();
}

#[tokio::test(start_paused = true)]
async fn app_and_installation_seeds_send_no_events() {
    let fixture = fixture(true).await;
    fixture
        .services
        .store
        .view(|tx| {
            assert!(tx.table::<Deliveries>().is_empty());
            Ok(())
        })
        .unwrap();
}
