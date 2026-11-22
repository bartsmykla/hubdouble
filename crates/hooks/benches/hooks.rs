use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use http::Uri;
use http_body_util::{BodyExt, Full};
use hubdouble_hooks::{ConnectFuture, Connector, Handle, Io, Options, emit};
use hubdouble_model::{
    Account, AccountKind, App, Config, Event, Installation, Repo, RepositorySelection, SystemClock,
    Timestamp, Visibility, Webhook, node_id,
};
use hubdouble_store::{Accounts, Apps, Installations, Repos, Store, StoreBuilder};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use serde_json::{Map, json};
use tokio::runtime::Runtime;

const ORG: i64 = 1;
const REPO: i64 = 2;
const APP: i64 = 3;
const INSTALLATION: i64 = 4;

struct SlowReceiver {
    delay: Duration,
}

impl Connector for SlowReceiver {
    fn connect(&self, _uri: &Uri) -> ConnectFuture {
        let delay = self.delay;
        Box::pin(async move {
            let (client, server) = tokio::io::duplex(1 << 16);
            tokio::spawn(async move {
                let svc = service_fn(move |req: Request<Incoming>| async move {
                    req.into_body().collect().await?;
                    tokio::time::sleep(delay).await;
                    Ok::<_, hyper::Error>(Response::new(Full::new(Bytes::new())))
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(server), svc)
                    .await;
            });
            Ok(Box::new(client) as Box<dyn Io>)
        })
    }
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

fn world() -> Store {
    let store = StoreBuilder::new().build();
    store
        .update_quiet(|tx| {
            tx.table_mut::<Accounts>()
                .insert(account(ORG, "acme", AccountKind::Organization))?;
            tx.table_mut::<Repos>().insert(Repo {
                id: REPO,
                node_id: node_id::entity(node_id::REPOSITORY, REPO),
                owner_id: ORG,
                name: "widget".to_owned(),
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
            })?;
            tx.table_mut::<Apps>().insert(App {
                id: APP,
                node_id: node_id::entity(node_id::APP, APP),
                slug: "bench".to_owned(),
                name: "bench".to_owned(),
                owner_id: ORG,
                bot_id: 99,
                description: None,
                external_url: String::new(),
                client_id: "Iv1.bench".to_owned(),
                client_secret: String::new(),
                public_key_pem: String::new(),
                private_key_pem: None,
                webhook: Some(Webhook {
                    url: "http://receiver.test/hook".to_owned(),
                    secret: Some("secret".to_owned()),
                    active: true,
                }),
                permissions: BTreeMap::new(),
                events: vec!["issue_comment".to_owned()],
                device_flow: false,
                created_at: Timestamp::SEED,
                updated_at: Timestamp::SEED,
            })?;
            tx.table_mut::<Installations>().insert(Installation {
                id: INSTALLATION,
                node_id: node_id::entity(node_id::INSTALLATION, INSTALLATION),
                app_id: APP,
                account_id: ORG,
                repository_selection: RepositorySelection::All,
                repo_ids: Vec::new(),
                permissions: BTreeMap::new(),
                events: Vec::new(),
                suspended_at: None,
                suspended_by: None,
                created_at: Timestamp::SEED,
                updated_at: Timestamp::SEED,
            })?;
            Ok(())
        })
        .unwrap_or_else(|e| panic!("{e:?}"));
    store
}

fn comment_event() -> Event {
    let mut payload = Map::new();
    payload.insert(
        "comment".to_owned(),
        json!({ "id": 1, "body": "/agent review", "user": { "login": "acme" } }),
    );
    Event {
        name: "issue_comment",
        action: Some("created"),
        repo_id: Some(REPO),
        org_id: None,
        sender_id: ORG,
        app_id: None,
        installation_id: None,
        payload,
    }
}

fn emit_events(store: &Store, n: usize) {
    for _ in 0..n {
        store
            .update(|tx| {
                emit(tx, comment_event());
                Ok(())
            })
            .unwrap_or_else(|e| panic!("{e:?}"));
    }
}

fn start(rt: &Runtime, store: &Store, delay: Duration, queue_capacity: usize) -> Handle {
    let _guard = rt.enter();
    Handle::start_with(
        store.clone(),
        Arc::new(SystemClock),
        &Config::default(),
        Options {
            queue_capacity,
            connector: Arc::new(SlowReceiver { delay }),
            ..Options::default()
        },
    )
    .unwrap_or_else(|e| panic!("{e:?}"))
}

fn enqueue(c: &mut Criterion) {
    let store = world();
    c.bench_function("emit_one_event", |b| {
        b.iter(|| {
            emit_events(&store, 1);
            store.take_outbox()
        });
    });
    emit_events(&store, 1);
    let bytes: usize = store
        .take_outbox()
        .iter()
        .map(|c| serde_json::to_vec(&c.entry.payload).map_or(0, |v| v.len()))
        .sum();
    eprintln!("outbox: one issue_comment entry holds a {bytes} byte payload");
}

fn throughput(c: &mut Criterion) {
    const EVENTS: usize = 200;
    let rt = Runtime::new().unwrap_or_else(|e| panic!("{e}"));
    let store = world();
    let hooks = start(&rt, &store, Duration::from_millis(1), 10_000);
    let mut g = c.benchmark_group("emit_to_delivered");
    g.throughput(Throughput::Elements(EVENTS as u64));
    g.sample_size(10);
    g.bench_function("slow_receiver_1ms", |b| {
        b.iter(|| {
            emit_events(&store, EVENTS);
            rt.block_on(hooks.drain())
        });
    });
    g.finish();
}

fn backlog(c: &mut Criterion) {
    const EVENTS: usize = 2_000;
    let rt = Runtime::new().unwrap_or_else(|e| panic!("{e}"));
    let store = world();
    let hooks = start(&rt, &store, Duration::from_millis(1), 64);
    let mut g = c.benchmark_group("backlog");
    g.throughput(Throughput::Elements(EVENTS as u64));
    g.sample_size(10);
    g.bench_function("writers_with_a_full_queue", |b| {
        b.iter_custom(|iters| {
            let mut spent = Duration::ZERO;
            for _ in 0..iters {
                let started = Instant::now();
                emit_events(&store, EVENTS);
                spent += started.elapsed();
                let _ = rt.block_on(hooks.drain());
            }
            spent
        });
    });
    g.finish();
    let stats = hooks.stats();
    eprintln!(
        "backlog: queue depth peaked at {} of 64 while {EVENTS} events were written",
        stats.max_queued
    );
}

criterion_group!(benches, enqueue, throughput, backlog);
criterion_main!(benches);
