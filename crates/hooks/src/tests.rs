use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use bytes::Bytes;
use http::Uri;
use http_body_util::{BodyExt, Full};
use hubdouble_model::{
    Access, Account, AccountKind, App, Config, Event, Installation, ManualClock, Repo,
    RepositorySelection, Timestamp, Visibility, Webhook, node_id,
};
use hubdouble_store::{
    Accounts, Apps, Counters, Deliveries, Installations, Repos, Sequence, Store, StoreBuilder,
};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use serde_json::{Map, Value, json};
use tokio::sync::{mpsc, oneshot};

use crate::delivery::{signature_1, signature_256};
use crate::{
    ConnectFuture, Connector, Handle, HooksError, Io, Notice, Options, emit, ping, redeliver,
};

const ORG: i64 = 10;
const USER: i64 = 11;
const WIDGET: i64 = 20;
const OTHER: i64 = 21;
const APP_A: i64 = 100;
const APP_B: i64 = 101;
const INSTALL_A: i64 = 200;
const INSTALL_B: i64 = 201;
const SECRET: &str = "s3cret";

struct Hit {
    headers: BTreeMap<String, String>,
    body: Bytes,
    reply: oneshot::Sender<u16>,
}

impl Hit {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap()
    }
}

fn json_of(body: &[u8]) -> Value {
    serde_json::from_slice(body).unwrap()
}

struct InProcess {
    hits: mpsc::UnboundedSender<Hit>,
    connects: Arc<AtomicUsize>,
}

impl Connector for InProcess {
    fn connect(&self, _uri: &Uri) -> ConnectFuture {
        self.connects.fetch_add(1, Ordering::SeqCst);
        let hits = self.hits.clone();
        Box::pin(async move {
            let (client, server) = tokio::io::duplex(1 << 16);
            tokio::spawn(async move {
                let svc = service_fn(move |req: Request<Incoming>| {
                    let hits = hits.clone();
                    async move {
                        let headers = req
                            .headers()
                            .iter()
                            .map(|(k, v)| (k.to_string(), v.to_str().unwrap().to_owned()))
                            .collect();
                        let body = req.into_body().collect().await?.to_bytes();
                        let (reply, answer) = oneshot::channel();
                        let _ = hits.send(Hit {
                            headers,
                            body,
                            reply,
                        });
                        let status = answer.await.unwrap_or(500);
                        Ok::<_, hyper::Error>(
                            Response::builder()
                                .status(status)
                                .body(Full::new(Bytes::from_static(b"received")))
                                .unwrap(),
                        )
                    }
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(server), svc)
                    .await;
            });
            Ok(Box::new(client) as Box<dyn Io>)
        })
    }
}

fn answer_all(mut hits: mpsc::UnboundedReceiver<Hit>) -> mpsc::UnboundedReceiver<Value> {
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(hit) = hits.recv().await {
            let body = hit.json();
            let _ = hit.reply.send(200);
            let _ = tx.send(body);
        }
    });
    rx
}

fn account(id: i64, login: &str, kind: AccountKind) -> Account {
    Account {
        id,
        node_id: node_id::entity(node_id::USER, id),
        login: login.to_owned(),
        kind,
        name: None,
        email: None,
        company: None,
        blog: None,
        location: None,
        bio: None,
        twitter_username: None,
        description: None,
        site_admin: false,
        verified_email: true,
        sso_enforced: false,
        app_id: None,
        created_at: Timestamp::SEED,
        updated_at: Timestamp::SEED,
    }
}

fn repo(id: i64, name: &str) -> Repo {
    Repo {
        id,
        node_id: node_id::entity(node_id::REPOSITORY, id),
        owner_id: ORG,
        name: name.to_owned(),
        description: None,
        homepage: None,
        language: None,
        topics: Vec::new(),
        visibility: Visibility::Private,
        fork_of: None,
        default_branch: "main".to_owned(),
        archived: false,
        disabled: false,
        is_template: false,
        has_issues: true,
        has_projects: true,
        has_wiki: true,
        has_discussions: false,
        allow_merge_commit: true,
        allow_squash_merge: true,
        allow_rebase_merge: true,
        allow_auto_merge: false,
        delete_branch_on_merge: false,
        collaborators: Vec::new(),
        created_at: Timestamp::SEED,
        updated_at: Timestamp::SEED,
        pushed_at: None,
    }
}

fn app(id: i64, slug: &str, events: &[&str]) -> App {
    App {
        id,
        node_id: node_id::entity(node_id::APP, id),
        slug: slug.to_owned(),
        name: slug.to_owned(),
        owner_id: ORG,
        bot_id: id + 1000,
        description: None,
        external_url: String::new(),
        client_id: format!("Iv1.{slug}"),
        client_secret: String::new(),
        public_key_pem: String::new(),
        private_key_pem: None,
        webhook: Some(Webhook {
            url: format!("http://{slug}.test/hook"),
            secret: Some(SECRET.to_owned()),
            active: true,
        }),
        permissions: BTreeMap::new(),
        events: events.iter().map(|e| (*e).to_owned()).collect(),
        device_flow: false,
        created_at: Timestamp::SEED,
        updated_at: Timestamp::SEED,
    }
}

fn installation(id: i64, app_id: i64, repos: Option<Vec<i64>>) -> Installation {
    Installation {
        id,
        node_id: node_id::entity(node_id::INSTALLATION, id),
        app_id,
        account_id: ORG,
        repository_selection: if repos.is_some() {
            RepositorySelection::Selected
        } else {
            RepositorySelection::All
        },
        repo_ids: repos.unwrap_or_default(),
        permissions: BTreeMap::from([
            ("contents".to_owned(), Access::Read),
            ("issues".to_owned(), Access::Read),
        ]),
        events: Vec::new(),
        suspended_at: None,
        suspended_by: None,
        created_at: Timestamp::SEED,
        updated_at: Timestamp::SEED,
    }
}

fn world() -> Store {
    let store = StoreBuilder::new().build_with(Arc::new(config()));
    store
        .update_quiet(|tx| {
            let mut accounts = tx.table_mut::<Accounts>();
            accounts.insert(account(ORG, "acme", AccountKind::Organization))?;
            accounts.insert(account(USER, "octo", AccountKind::User))?;
            tx.table_mut::<Repos>().insert(repo(WIDGET, "widget"))?;
            tx.table_mut::<Repos>().insert(repo(OTHER, "other"))?;
            tx.table_mut::<Apps>()
                .insert(app(APP_A, "alpha", &["push", "issue_comment"]))?;
            tx.table_mut::<Apps>()
                .insert(app(APP_B, "beta", &["push"]))?;
            tx.table_mut::<Installations>()
                .insert(installation(INSTALL_A, APP_A, None))?;
            tx.table_mut::<Installations>().insert(installation(
                INSTALL_B,
                APP_B,
                Some(vec![OTHER]),
            ))?;
            Ok(())
        })
        .unwrap();
    store
}

fn event(name: &'static str, repo_id: i64) -> Event {
    Event {
        name,
        action: None,
        repo_id: Some(repo_id),
        org_id: None,
        sender_id: USER,
        app_id: None,
        installation_id: None,
        payload: Map::new(),
    }
}

fn emit_one(store: &Store, e: Event) {
    store
        .update(|tx| {
            emit(tx, e);
            Ok(())
        })
        .unwrap();
}

fn config() -> Config {
    Config {
        base_url: "http://hubdouble.test".to_owned(),
        web_url: "https://github.com".to_owned(),
        ..Config::default()
    }
}

fn start(store: &Store, tune: impl FnOnce(&mut Options)) -> (Handle, mpsc::UnboundedReceiver<Hit>) {
    let (hits, rx) = mpsc::unbounded_channel();
    let mut options = Options {
        connector: Arc::new(InProcess {
            hits,
            connects: Arc::default(),
        }),
        ..Options::default()
    };
    tune(&mut options);
    let clock = Arc::new(ManualClock::new(Timestamp::SEED));
    let handle = Handle::start_with(store.clone(), clock, options).unwrap();
    (handle, rx)
}

fn logged_ids(store: &Store, app_id: i64) -> Vec<i64> {
    store
        .view(|tx| {
            Ok(tx
                .table::<Deliveries>()
                .children(app_id)
                .map(|d| d.id)
                .collect())
        })
        .unwrap()
}

#[test]
fn recipients_follow_installation_coverage_and_subscription() {
    let store = world();
    let cases = [
        ("push", WIDGET, vec![APP_A]),
        ("push", OTHER, vec![APP_A, APP_B]),
        ("issue_comment", OTHER, vec![APP_A]),
        ("release", OTHER, vec![]),
    ];
    for (name, repo_id, want) in cases {
        // Given the fixture installations
        // When an event on the repository commits
        emit_one(&store, event(name, repo_id));
        // Then exactly the covering, subscribed Apps get an entry
        let mut got: Vec<i64> = store.take_outbox().iter().map(|c| c.entry.app_id).collect();
        got.sort_unstable();
        assert_eq!(got, want, "{name} on {repo_id}");
    }
}

#[test]
fn an_event_reaches_only_installations_granted_its_permission() {
    // Given two Apps that subscribe to the same events on one repository, and
    // installation B with contents and checks read but no issues permission
    let store = world();
    store
        .update_quiet(|tx| {
            let events = ["push", "issue_comment", "check_run", "label", "deployment"];
            tx.table_mut::<Apps>()
                .replace(app(APP_A, "alpha", &events))?;
            tx.table_mut::<Apps>()
                .replace(app(APP_B, "beta", &events))?;
            let mut a = installation(INSTALL_A, APP_A, None);
            a.permissions.insert("checks".to_owned(), Access::Write);
            let mut b = installation(INSTALL_B, APP_B, Some(vec![OTHER]));
            b.permissions = BTreeMap::from([
                ("contents".to_owned(), Access::Read),
                ("checks".to_owned(), Access::Read),
            ]);
            tx.table_mut::<Installations>().replace(a)?;
            tx.table_mut::<Installations>().replace(b)?;
            Ok(())
        })
        .unwrap();
    let cases = [
        ("issue_comment", None, vec![APP_A]),
        ("push", None, vec![APP_A, APP_B]),
        ("check_run", Some("created"), vec![APP_A, APP_B]),
        ("check_run", Some("requested_action"), vec![APP_A]),
        ("label", Some("created"), vec![APP_A, APP_B]),
        ("deployment", Some("created"), vec![]),
    ];
    for (name, action, want) in cases {
        // When the event on the repository commits
        emit_one(
            &store,
            Event {
                action,
                ..event(name, OTHER)
            },
        );
        // Then only installations with the event's permission at the needed level get it
        let mut got: Vec<i64> = store.take_outbox().iter().map(|c| c.entry.app_id).collect();
        got.sort_unstable();
        assert_eq!(got, want, "{name} {action:?}");
    }
}

#[test]
fn an_app_with_checks_write_gets_check_events_without_subscribing() {
    // Given App A with checks write and App B with checks read, neither subscribed to check
    // events, and installations that grant both checks write
    let store = world();
    store
        .update_quiet(|tx| {
            let mut a = app(APP_A, "alpha", &["push"]);
            a.permissions.insert("checks".to_owned(), Access::Write);
            let mut b = app(APP_B, "beta", &["push"]);
            b.permissions.insert("checks".to_owned(), Access::Read);
            tx.table_mut::<Apps>().replace(a)?;
            tx.table_mut::<Apps>().replace(b)?;
            let mut ia = installation(INSTALL_A, APP_A, None);
            ia.permissions.insert("checks".to_owned(), Access::Write);
            let mut ib = installation(INSTALL_B, APP_B, Some(vec![OTHER]));
            ib.permissions.insert("checks".to_owned(), Access::Write);
            tx.table_mut::<Installations>().replace(ia)?;
            tx.table_mut::<Installations>().replace(ib)?;
            Ok(())
        })
        .unwrap();
    for (name, action) in [
        ("check_run", Some("created")),
        ("check_run", Some("requested_action")),
        ("check_suite", Some("completed")),
        ("check_suite", Some("rerequested")),
    ] {
        // When the check event on the repository commits
        emit_one(
            &store,
            Event {
                action,
                ..event(name, OTHER)
            },
        );
        // Then only the App with checks write gets it
        let got: Vec<i64> = store.take_outbox().iter().map(|c| c.entry.app_id).collect();
        assert_eq!(got, [APP_A], "{name} {action:?}");
    }
    // And an installation that grants only checks read still misses the write-level actions
    store
        .update_quiet(|tx| {
            let mut ia = installation(INSTALL_A, APP_A, None);
            ia.permissions.insert("checks".to_owned(), Access::Read);
            tx.table_mut::<Installations>().replace(ia)?;
            Ok(())
        })
        .unwrap();
    for (action, want) in [("created", vec![APP_A]), ("requested_action", vec![])] {
        emit_one(
            &store,
            Event {
                action: Some(action),
                ..event("check_run", OTHER)
            },
        );
        let got: Vec<i64> = store.take_outbox().iter().map(|c| c.entry.app_id).collect();
        assert_eq!(got, want, "check_run {action}");
    }
}

#[test]
fn installation_event_reaches_only_its_app_with_the_data_before_the_change() {
    // Given two Apps installed on one account
    let store = world();
    // When one installation is deleted in the transaction that emits its event
    store
        .update(|tx| {
            emit(
                tx,
                Event {
                    action: Some("deleted"),
                    app_id: Some(APP_A),
                    installation_id: Some(INSTALL_A),
                    repo_id: None,
                    org_id: Some(ORG),
                    ..event("installation", WIDGET)
                },
            );
            tx.table_mut::<Installations>().remove(INSTALL_A);
            Ok(())
        })
        .unwrap();
    // Then only that App gets it, carrying the deleted installation
    let outbox = store.take_outbox();
    assert_eq!(outbox.len(), 1);
    let entry = &outbox[0].entry;
    assert_eq!(entry.app_id, APP_A);
    assert_eq!(entry.installation_id, Some(INSTALL_A));
    let payload = json_of(&entry.payload);
    assert_eq!(payload["installation"]["id"], json!(INSTALL_A));
    assert_eq!(
        payload["installation"]["node_id"],
        json!(node_id::entity(node_id::INSTALLATION, INSTALL_A))
    );
    assert_eq!(payload["organization"]["login"], json!("acme"));
}

#[test]
fn signatures_match_known_vectors() {
    assert_eq!(
        signature_256(b"It's a Secret to Everybody", b"Hello, World!"),
        "sha256=757107ea0eb2509fc211221cce984b8a37570b6d7586c22c46f4379c8b043e17"
    );
    assert_eq!(
        signature_1(b"Jefe", b"what do ya want for nothing?"),
        "sha1=effcdf6ae5eb2fa2d27416d5f184df9c259a7c79"
    );
}

#[tokio::test(start_paused = true)]
async fn delivers_signed_with_githubs_headers_and_logs_the_attempt() {
    let store = world();
    let (hooks, mut hits) = start(&store, |_| {});
    // Given an event whose payload already names its sender
    let mut e = event("issue_comment", WIDGET);
    e.action = Some("created");
    e.payload
        .insert("comment".to_owned(), json!({ "id": 7, "body": "hi" }));
    e.payload
        .insert("sender".to_owned(), json!({ "login": "given" }));
    emit_one(&store, e);
    // When the receiver answers the attempt
    let hit = hits.recv().await.unwrap();
    let h = hit.headers.clone();
    let body = hit.json();
    let raw = hit.body.clone();
    hit.reply.send(202).unwrap();
    hooks.drain().await.unwrap();
    // Then the request carries GitHub's headers and a valid signature
    let guid = &h["x-github-delivery"];
    assert_eq!(guid.len(), 36);
    assert!(
        guid.chars()
            .all(|c| c == '-' || c.is_ascii_digit() || ('a'..='f').contains(&c))
    );
    assert_eq!(h["x-github-event"], "issue_comment");
    assert_eq!(h["x-github-hook-id"], APP_A.to_string());
    assert_eq!(h["x-github-hook-installation-target-type"], "integration");
    assert_eq!(h["x-github-hook-installation-target-id"], APP_A.to_string());
    assert_eq!(h["content-type"], "application/json");
    assert!(h["user-agent"].starts_with("GitHub-Hookshot/"));
    assert_eq!(
        h["x-hub-signature-256"],
        signature_256(SECRET.as_bytes(), &raw)
    );
    assert_eq!(h["x-hub-signature"], signature_1(SECRET.as_bytes(), &raw));
    // And the payload has its context with the configured origins
    assert_eq!(body["action"], json!("created"));
    assert_eq!(body["comment"]["id"], json!(7));
    assert_eq!(body["sender"], json!({ "login": "given" }));
    assert_eq!(body["installation"]["id"], json!(INSTALL_A));
    assert_eq!(body["organization"]["login"], json!("acme"));
    assert_eq!(body["repository"]["full_name"], json!("acme/widget"));
    // And the delivery log holds the attempt
    let d = store
        .view(|tx| Ok(tx.table::<Deliveries>().get(1).cloned()))
        .unwrap()
        .unwrap();
    assert_eq!(d.guid, *guid);
    assert_eq!(d.app_id, APP_A);
    assert_eq!(d.installation_id, Some(INSTALL_A));
    assert_eq!(d.repo_id, Some(WIDGET));
    assert_eq!(d.event, "issue_comment");
    assert_eq!(d.action.as_deref(), Some("created"));
    assert_eq!(d.status_code, 202);
    assert!(!d.redelivery);
    assert_eq!(d.delivered_at, Timestamp::SEED);
    assert_eq!(json_of(&d.request.payload), body);
    assert_eq!(d.request.headers["X-GitHub-Delivery"], *guid);
    assert_eq!(d.response.payload.as_deref(), Some("received"));
}

fn values_under(v: &Value, key: &str, out: &mut Vec<String>) {
    match v {
        Value::Object(fields) => {
            for (k, f) in fields {
                if k == key
                    && let Some(s) = f.as_str()
                {
                    out.push(s.to_owned());
                }
                values_under(f, key, out);
            }
        }
        Value::Array(items) => items.iter().for_each(|i| values_under(i, key, out)),
        _ => {}
    }
}

#[tokio::test(start_paused = true)]
async fn delivered_payload_urls_use_the_configured_origins() {
    let store = world();
    let (hooks, hits) = start(&store, |_| {});
    let mut bodies = answer_all(hits);
    // Given a server with a base URL and a separate web URL
    // When an event on a repository of an org is delivered
    emit_one(&store, event("issue_comment", WIDGET));
    hooks.drain().await.unwrap();
    let body = bodies.recv().await.unwrap();
    // Then every API url uses the base URL and every html_url the web URL
    let (mut urls, mut html) = (Vec::new(), Vec::new());
    values_under(&body, "url", &mut urls);
    values_under(&body, "html_url", &mut html);
    assert!(urls.contains(&"http://hubdouble.test/repos/acme/widget".to_owned()));
    assert!(urls.contains(&"http://hubdouble.test/orgs/acme".to_owned()));
    assert!(urls.contains(&"http://hubdouble.test/users/octo".to_owned()));
    assert!(
        urls.iter().all(|u| u.starts_with("http://hubdouble.test/")),
        "{urls:?}"
    );
    assert!(html.contains(&"https://github.com/acme/widget".to_owned()));
    assert!(
        html.iter().all(|u| u.starts_with("https://github.com/")),
        "{html:?}"
    );
    assert_eq!(
        body["repository"]["ssh_url"],
        json!("git@github.com:acme/widget.git")
    );
}

#[tokio::test(start_paused = true)]
async fn redelivery_keeps_the_guid_and_gets_a_new_id() {
    let store = world();
    let (hooks, hits) = start(&store, |_| {});
    let mut bodies = answer_all(hits);
    emit_one(&store, event("push", WIDGET));
    hooks.drain().await.unwrap();
    let first = bodies.recv().await.unwrap();
    // When delivery 1 is redelivered
    store
        .update(|tx| redeliver(tx, APP_A, 1).map_err(Into::into))
        .unwrap();
    hooks.drain().await.unwrap();
    // Then the same payload goes out under the same GUID, logged as delivery 2
    assert_eq!(bodies.recv().await.unwrap(), first);
    let (one, two) = store
        .view(|tx| {
            let t = tx.table::<Deliveries>();
            Ok((t.get(1).cloned().unwrap(), t.get(2).cloned().unwrap()))
        })
        .unwrap();
    assert_eq!(two.guid, one.guid);
    assert_eq!(two.event, "push");
    assert!(two.redelivery);
    assert!(!one.redelivery);
    // And another App cannot redeliver it
    let other = store.update(|tx| Ok(redeliver(tx, APP_B, 1)));
    assert_eq!(other.unwrap(), Err(HooksError::UnknownDelivery(1)));
}

#[tokio::test(start_paused = true)]
async fn two_concurrent_writers_are_delivered_in_commit_order() {
    let store = world();
    let (hooks, hits) = start(&store, |o| o.workers = 1);
    let mut bodies = answer_all(hits);
    // Given two writers that each commit 50 events, numbered under the store lock
    std::thread::scope(|s| {
        for _ in 0..2 {
            let store = store.clone();
            s.spawn(move || {
                for _ in 0..50 {
                    store
                        .update(|tx| {
                            let n = tx.table_mut::<Counters>().next(Sequence::Run);
                            let mut e = event("push", WIDGET);
                            e.payload.insert("n".to_owned(), json!(n));
                            emit(tx, e);
                            Ok(())
                        })
                        .unwrap();
                }
            });
        }
    });
    hooks.drain().await.unwrap();
    // Then the receiver sees them in commit order, and delivery IDs follow it
    let mut seen = Vec::new();
    while let Ok(b) = bodies.try_recv() {
        seen.push(b["n"].as_i64().unwrap());
    }
    assert_eq!(seen, (1..=100).collect::<Vec<_>>());
    assert_eq!(logged_ids(&store, APP_A), (1..=100).collect::<Vec<_>>());
}

#[tokio::test(start_paused = true)]
async fn the_log_shows_an_attempt_only_after_every_lower_id_finished() {
    let store = world();
    let (hooks, mut hits) = start(&store, |o| o.workers = 2);
    for n in 1..=2 {
        let mut e = event("push", WIDGET);
        e.payload.insert("n".to_owned(), json!(n));
        emit_one(&store, e);
    }
    let a = hits.recv().await.unwrap();
    let b = hits.recv().await.unwrap();
    let (first, second) = if a.json()["n"] == json!(1) {
        (a, b)
    } else {
        (b, a)
    };
    // When delivery 2 finishes while delivery 1 is in flight
    second.reply.send(200).unwrap();
    hooks.shared.wait_stats(|s| s.in_flight == 1).await;
    // Then the log shows neither
    assert_eq!(logged_ids(&store, APP_A), Vec::<i64>::new());
    // When delivery 1 finishes too
    first.reply.send(200).unwrap();
    hooks.drain().await.unwrap();
    // Then both are listed, each under the ID it got at enqueue
    assert_eq!(logged_ids(&store, APP_A), vec![1, 2]);
    let n_of = |id: i64| {
        store
            .view(|tx| {
                Ok(
                    json_of(&tx.table::<Deliveries>().get(id).unwrap().request.payload)["n"]
                        .clone(),
                )
            })
            .unwrap()
    };
    assert_eq!((n_of(1), n_of(2)), (json!(1), json!(2)));
}

#[tokio::test(start_paused = true)]
async fn an_attempt_times_out_after_ten_seconds() {
    let store = world();
    let (hooks, mut hits) = start(&store, |_| {});
    emit_one(&store, event("push", WIDGET));
    // Given a receiver that never answers
    let _held = hits.recv().await.unwrap();
    // When the timeout passes
    hooks.drain().await.unwrap();
    // Then the attempt is logged as failed after GitHub's 10 s
    let d = store
        .view(|tx| Ok(tx.table::<Deliveries>().get(1).cloned()))
        .unwrap()
        .unwrap();
    assert_eq!(d.status_code, 0);
    assert!((d.duration - 10.0).abs() < 1e-3, "{}", d.duration);
}

#[tokio::test(start_paused = true)]
async fn an_event_past_the_byte_budget_is_logged_failed_without_an_attempt() {
    let store = world();
    let sizing = world();
    emit_one(&sizing, event("push", WIDGET));
    let one = sizing.take_outbox()[0].entry.payload.len();
    // Given a budget of one payload and a receiver that holds the first attempt
    let (seen, mut notices) = mpsc::unbounded_channel();
    let (hooks, mut hits) = start(&store, |o| {
        o.workers = 1;
        o.queue_bytes = one;
        o.observer = Some(Arc::new(move |n: &Notice<'_>| {
            if let Notice::Delivery {
                delivery_id,
                status_code,
                ..
            } = *n
            {
                let _ = seen.send((delivery_id, status_code));
            }
        }));
    });
    emit_one(&store, event("push", WIDGET));
    // When a second event commits while the first still waits for its attempt
    emit_one(&store, event("push", WIDGET));
    let first = hits.recv().await.unwrap();
    first.reply.send(200).unwrap();
    hooks.drain().await.unwrap();
    // Then the receiver saw one request, and the second is a failed delivery that kept its body
    assert!(hits.try_recv().is_err());
    assert_eq!(logged_ids(&store, APP_A), vec![1, 2]);
    let d = store
        .view(|tx| Ok(tx.table::<Deliveries>().get(2).cloned()))
        .unwrap()
        .unwrap();
    assert_eq!(d.status_code, 0);
    assert_eq!(d.event, "push");
    assert!(d.response.payload.is_none());
    assert_eq!(
        json_of(&d.request.payload)["repository"]["id"],
        json!(WIDGET)
    );
    assert!(d.request.headers.contains_key("X-Hub-Signature-256"));
    // And the observer saw both deliveries, the refused one with status_code 0
    let mut observed = Vec::new();
    while let Ok(n) = notices.try_recv() {
        observed.push(n);
    }
    assert_eq!(observed, vec![(1, 200), (2, 0)]);
    // And once the budget is free again, a new event is delivered
    let mut bodies = answer_all(hits);
    emit_one(&store, event("push", WIDGET));
    hooks.drain().await.unwrap();
    assert!(bodies.recv().await.is_some());
    assert_eq!(hooks.stats().waiting_bytes, 0);
}

#[tokio::test(start_paused = true)]
async fn in_flight_attempts_keep_their_share_of_the_queue_byte_budget() {
    let large = |n: i64| {
        let mut e = event("push", WIDGET);
        e.payload.insert("n".to_owned(), json!(n));
        e.payload
            .insert("filler".to_owned(), json!("x".repeat(64 * 1024)));
        e
    };
    let store = world();
    let sizing = world();
    emit_one(&sizing, large(1));
    let one = sizing.take_outbox()[0].entry.payload.len();
    // Given a budget of two large payloads, four workers and a receiver that answers nothing yet
    let (hooks, hits) = start(&store, |o| {
        o.workers = 4;
        o.queue_bytes = 2 * one;
    });
    // When six large events commit, each after the one before it was taken or refused
    for n in 1..=6 {
        emit_one(&store, large(n));
        hooks
            .shared
            .wait_stats(|s| {
                s.unlogged == usize::try_from(n).unwrap() && (s.queued == 0 || s.in_flight == 4)
            })
            .await;
    }
    // Then the events in flight or queued never hold more than the budget
    let s = hooks.stats();
    assert!(s.in_flight + s.queued <= 2, "{s:?}");
    assert!(s.waiting_bytes <= 2 * one, "{s:?}");
    // And once the receiver answers, the two sent events succeed and the rest are Queue full
    let _bodies = answer_all(hits);
    hooks.drain().await.unwrap();
    let codes: Vec<i64> = store
        .view(|tx| {
            Ok(tx
                .table::<Deliveries>()
                .children(APP_A)
                .map(|d| d.status_code)
                .collect())
        })
        .unwrap();
    assert_eq!(codes, [200, 200, 0, 0, 0, 0]);
    assert_eq!(hooks.stats().waiting_bytes, 0);
}

#[tokio::test(start_paused = true)]
async fn past_the_log_byte_budget_the_oldest_deliveries_lose_their_bodies() {
    let numbered = |n: i64| {
        let mut e = event("push", WIDGET);
        e.payload.insert("n".to_owned(), json!(n));
        e
    };
    let store = world();
    let sizing = world();
    let (sized, hits) = start(&sizing, |_| {});
    let _sized_bodies = answer_all(hits);
    emit_one(&sizing, numbered(1));
    sized.drain().await.unwrap();
    let d = sizing
        .view(|tx| Ok(tx.table::<Deliveries>().get(1).cloned()))
        .unwrap()
        .unwrap();
    let headers =
        |h: &BTreeMap<String, String>| -> usize { h.iter().map(|(k, v)| k.len() + v.len()).sum() };
    let one = d.request.payload.len()
        + headers(&d.request.headers)
        + d.response.payload.as_ref().map_or(0, String::len)
        + headers(&d.response.headers);
    let limit = 2 * one;
    // Given a log budget of two delivered requests and responses, and three workers
    let (hooks, mut hits) = start(&store, |o| {
        o.workers = 3;
        o.log_bytes = limit;
    });
    for n in 1..=3 {
        emit_one(&store, numbered(n));
    }
    let mut by_n = BTreeMap::new();
    for _ in 0..3 {
        let hit = hits.recv().await.unwrap();
        by_n.insert(hit.json()["n"].as_i64().unwrap(), hit);
    }
    // When deliveries 2 and 3 finish while delivery 1 is still in flight
    by_n.remove(&2).unwrap().reply.send(200).unwrap();
    by_n.remove(&3).unwrap().reply.send(200).unwrap();
    hooks.shared.wait_stats(|s| s.in_flight == 1).await;
    // Then the bodies waiting behind delivery 1 count toward the budget
    assert!(hooks.stats().log_bytes <= limit);
    // When delivery 1 finishes and a fourth event is delivered
    by_n.remove(&1).unwrap().reply.send(200).unwrap();
    hooks.drain().await.unwrap();
    let _bodies = answer_all(hits);
    emit_one(&store, numbered(4));
    hooks.drain().await.unwrap();
    // Then every delivery is still listed with its status, the oldest ones without bodies
    assert_eq!(logged_ids(&store, APP_A), vec![1, 2, 3, 4]);
    let all: Vec<_> = store
        .view(|tx| Ok(tx.table::<Deliveries>().children(APP_A).cloned().collect()))
        .unwrap();
    let kept: Vec<bool> = all
        .iter()
        .map(|d| {
            let full = !d.request.payload.is_empty() && d.response.payload.is_some();
            assert_eq!(d.request.headers.is_empty(), !full);
            full
        })
        .collect();
    assert_eq!(kept, vec![false, false, true, true]);
    assert!(
        all.iter()
            .all(|d| d.status_code == 200 && d.event == "push")
    );
    assert!(hooks.stats().log_bytes <= limit);
    // And a delivery without its body cannot be redelivered
    let again = store.update(|tx| Ok(redeliver(tx, APP_A, 1)));
    assert_eq!(again.unwrap(), Err(HooksError::UnknownDelivery(1)));
}

#[tokio::test(start_paused = true)]
async fn reset_stops_queued_and_in_flight_attempts_of_the_old_epoch() {
    let store = world();
    let snapshot = store.snapshot();
    let (hooks, mut hits) = start(&store, |o| o.workers = 1);
    // Given one attempt in flight and two queued
    for _ in 0..3 {
        emit_one(&store, event("push", WIDGET));
    }
    let in_flight = hits.recv().await.unwrap();
    hooks
        .shared
        .wait_stats(|s| s.in_flight == 1 && s.queued == 2)
        .await;
    // When reset runs and the in-flight attempt then ends
    let reset = tokio::spawn({
        let hooks = hooks.clone();
        async move { hooks.reset(&snapshot).await }
    });
    hooks.shared.wait_stats(|s| s.queued == 0).await;
    assert!(!reset.is_finished());
    in_flight.reply.send(200).unwrap();
    reset.await.unwrap().unwrap();
    // Then nothing more reaches the receiver and nothing is logged
    hooks.drain().await.unwrap();
    assert!(hits.try_recv().is_err());
    assert_eq!(logged_ids(&store, APP_A), Vec::<i64>::new());
    assert_eq!(hooks.stats().waiting_bytes, 0);
    // And a new event is delivered with the next delivery ID
    let mut bodies = answer_all(hits);
    emit_one(&store, event("push", WIDGET));
    hooks.drain().await.unwrap();
    assert!(bodies.recv().await.is_some());
    assert_eq!(logged_ids(&store, APP_A), vec![4]);
}

#[tokio::test(start_paused = true)]
async fn the_log_keeps_only_the_first_response_bytes_of_a_long_answer() {
    let store = world();
    // Given a cap shorter than the receiver's 8-byte answer "received"
    let (hooks, hits) = start(&store, |o| o.response_bytes = 3);
    let _bodies = answer_all(hits);
    // When an event is delivered
    emit_one(&store, event("push", WIDGET));
    hooks.drain().await.unwrap();
    // Then the attempt succeeds and the log keeps only the capped prefix
    let d = store
        .view(|tx| Ok(tx.table::<Deliveries>().get(1).cloned()))
        .unwrap()
        .unwrap();
    assert_eq!(d.status_code, 200);
    assert_eq!(d.response.payload.as_deref(), Some("rec"));
}

#[tokio::test(start_paused = true)]
async fn reset_keeps_a_job_the_new_epoch_queued_before_it_took_the_queue() {
    let store = world();
    let snapshot = store.snapshot();
    let (hooks, mut hits) = start(&store, |o| o.workers = 1);
    emit_one(&store, event("push", WIDGET));
    let in_flight = hits.recv().await.unwrap();
    // Given reset has moved the store to a new epoch
    store.restore(&snapshot);
    // And an event of the new epoch is queued before reset takes the queue
    emit_one(&store, event("push", WIDGET));
    hooks.shared.wait_stats(|s| s.queued == 1).await;
    // When reset forgets the older epoch
    let forget = tokio::spawn({
        let hooks = hooks.clone();
        let epoch = store.epoch();
        async move { hooks.shared.forget_older(epoch).await }
    });
    in_flight.reply.send(200).unwrap();
    forget.await.unwrap().unwrap();
    // Then the new event is still delivered and logged
    let _bodies = answer_all(hits);
    hooks.drain().await.unwrap();
    assert_eq!(logged_ids(&store, APP_A), vec![2]);
}

#[tokio::test(start_paused = true)]
async fn an_earlier_reset_keeps_the_work_of_a_later_one() {
    let store = world();
    let snapshot = store.snapshot();
    let (hooks, mut hits) = start(&store, |o| o.workers = 1);
    emit_one(&store, event("push", WIDGET));
    let in_flight = hits.recv().await.unwrap();
    // Given a first reset that moved to an epoch and a second reset that moved past it
    store.restore(&snapshot);
    let first = store.epoch();
    store.restore(&snapshot);
    // And an event of the second reset's epoch waits in the queue
    emit_one(&store, event("push", WIDGET));
    hooks.shared.wait_stats(|s| s.queued == 1).await;
    // When the first reset forgets the epochs before its own
    let forget = tokio::spawn({
        let hooks = hooks.clone();
        async move { hooks.shared.forget_older(first).await }
    });
    in_flight.reply.send(200).unwrap();
    forget.await.unwrap().unwrap();
    // Then the later epoch's event is still delivered and logged
    let _bodies = answer_all(hits);
    hooks.drain().await.unwrap();
    assert_eq!(logged_ids(&store, APP_A), vec![2]);
}

#[tokio::test(start_paused = true)]
async fn attempts_reuse_keep_alive_connections_up_to_the_worker_count() {
    let store = world();
    let connects = Arc::new(AtomicUsize::new(0));
    let (hits, rx) = mpsc::unbounded_channel();
    let options = Options {
        workers: 2,
        connector: Arc::new(InProcess {
            hits,
            connects: connects.clone(),
        }),
        ..Options::default()
    };
    let clock = Arc::new(ManualClock::new(Timestamp::SEED));
    let hooks = Handle::start_with(store.clone(), clock, options).unwrap();
    let mut bodies = answer_all(rx);
    // Given two workers and a receiver that keeps its connections open
    // When twenty events are delivered
    for _ in 0..20 {
        emit_one(&store, event("push", WIDGET));
    }
    hooks.drain().await.unwrap();
    // Then each reached the receiver over at most one connection per worker
    for _ in 0..20 {
        assert!(bodies.recv().await.is_some());
    }
    assert!(connects.load(Ordering::SeqCst) <= 2);
    assert_eq!(logged_ids(&store, APP_A).len(), 20);
}

#[tokio::test(start_paused = true)]
async fn an_attempt_after_the_idle_limit_opens_a_new_connection() {
    let store = world();
    let connects = Arc::new(AtomicUsize::new(0));
    let (hits, rx) = mpsc::unbounded_channel();
    let options = Options {
        workers: 1,
        connector: Arc::new(InProcess {
            hits,
            connects: connects.clone(),
        }),
        ..Options::default()
    };
    let clock = Arc::new(ManualClock::new(Timestamp::SEED));
    let hooks = Handle::start_with(store.clone(), clock, options).unwrap();
    let mut bodies = answer_all(rx);
    // Given one delivery whose connection went back to the pool
    emit_one(&store, event("push", WIDGET));
    hooks.drain().await.unwrap();
    assert!(bodies.recv().await.is_some());
    // When the next attempt starts just inside and then just past the idle limit
    tokio::time::advance(crate::delivery::IDLE_REUSE.saturating_sub(Duration::from_secs(1))).await;
    emit_one(&store, event("push", WIDGET));
    hooks.drain().await.unwrap();
    assert!(bodies.recv().await.is_some());
    assert_eq!(connects.load(Ordering::SeqCst), 1);
    tokio::time::advance(crate::delivery::IDLE_REUSE).await;
    emit_one(&store, event("push", WIDGET));
    hooks.drain().await.unwrap();
    assert!(bodies.recv().await.is_some());
    // Then only the attempt past the limit opened a second connection
    assert_eq!(connects.load(Ordering::SeqCst), 2);
}

#[tokio::test(start_paused = true)]
async fn the_delivery_log_is_a_ring_per_app() {
    let store = world();
    let (hooks, hits) = start(&store, |o| o.log_capacity = 2);
    let _bodies = answer_all(hits);
    for repo_id in [WIDGET, WIDGET, OTHER] {
        emit_one(&store, event("push", repo_id));
    }
    hooks.drain().await.unwrap();
    assert_eq!(logged_ids(&store, APP_A), vec![2, 3]);
    assert_eq!(logged_ids(&store, APP_B), vec![4]);
}

#[tokio::test(start_paused = true)]
async fn ping_describes_the_apps_hook() {
    let store = world();
    let (hooks, hits) = start(&store, |_| {});
    let mut bodies = answer_all(hits);
    store
        .update(|tx| {
            ping(tx, APP_B);
            Ok(())
        })
        .unwrap();
    hooks.drain().await.unwrap();
    let body = bodies.recv().await.unwrap();
    assert_eq!(body["hook_id"], json!(APP_B));
    assert_eq!(body["hook"]["type"], json!("App"));
    assert_eq!(body["hook"]["app_id"], json!(APP_B));
    assert_eq!(body["hook"]["events"], json!(["push"]));
    assert_eq!(
        body["hook"]["config"]["url"],
        json!("http://beta.test/hook")
    );
    assert_eq!(
        body["hook"]["url"],
        json!("http://hubdouble.test/app/hook/config")
    );
    assert_eq!(
        body["hook"]["ping_url"],
        json!("http://hubdouble.test/app/hook/pings")
    );
    assert_eq!(
        body["hook"]["deliveries_url"],
        json!("http://hubdouble.test/app/hook/deliveries")
    );
    assert!(body["zen"].is_string());
    let d = store
        .view(|tx| Ok(tx.table::<Deliveries>().get(1).cloned()))
        .unwrap()
        .unwrap();
    assert_eq!((d.event.as_str(), d.app_id), ("ping", APP_B));
}
