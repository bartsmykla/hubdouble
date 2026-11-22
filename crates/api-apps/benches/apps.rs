use std::collections::BTreeMap;
use std::hint::black_box;
use std::sync::Arc;

use bytes::Bytes;
use criterion::{Criterion, criterion_group, criterion_main};
use http::{HeaderMap, Method};
use hubdouble_admin::{Admin, Registrar};
use hubdouble_auth::Actor;
use hubdouble_git::GitBuilder;
use hubdouble_model::{
    Account, AccountKind, ApiError, App, Delivery, DeliveryRequest, DeliveryResponse, Installation,
    ManualClock, RepositorySelection, Timestamp, node_id,
};
use hubdouble_render::Urls;
use hubdouble_rest::{Access, Ctx, Parts, Query, Registry};
use hubdouble_store::{Accounts, Apps, Deliveries, Installations, Store, StoreBuilder};
use serde_json::json;

const OWNER: i64 = 1;
const APP: i64 = 2;
const INSTALL: i64 = 3;

fn prepare(store: &Store) -> Result<Actor, ApiError> {
    let now = Timestamp::SEED;
    store.update(|tx| {
        tx.table_mut::<Accounts>().insert(Account {
            id: OWNER,
            node_id: node_id::entity(node_id::ORGANIZATION, OWNER),
            login: "example".into(),
            kind: AccountKind::Organization,
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
            created_at: now,
            updated_at: now,
        })?;
        let app = tx.table_mut::<Apps>().insert(App {
            id: APP,
            node_id: node_id::entity(node_id::APP, APP),
            slug: "demo".into(),
            name: "demo".into(),
            owner_id: OWNER,
            bot_id: 4,
            description: None,
            external_url: "https://example.com".into(),
            client_id: "demo-client".into(),
            client_secret: "demo-secret".into(),
            public_key_pem: String::new(),
            private_key_pem: None,
            webhook: None,
            permissions: BTreeMap::new(),
            events: vec!["issue_comment".into()],
            device_flow: false,
            created_at: now,
            updated_at: now,
        })?;
        tx.table_mut::<Installations>().insert(Installation {
            id: INSTALL,
            node_id: node_id::entity(node_id::INSTALLATION, INSTALL),
            app_id: APP,
            account_id: OWNER,
            repository_selection: RepositorySelection::All,
            repo_ids: Vec::new(),
            permissions: BTreeMap::new(),
            events: app.events.clone(),
            suspended_at: None,
            suspended_by: None,
            created_at: now,
            updated_at: now,
        })?;
        let mut deliveries = tx.table_mut::<Deliveries>();
        for id in 1..=1000 {
            deliveries.insert(delivery(id))?;
        }
        Ok(Actor::App { app })
    })
}

fn delivery(id: i64) -> Delivery {
    let status_code = if id % 4 == 0 { 503 } else { 200 };
    Delivery {
        id,
        guid: format!("00000000-0000-4000-8000-{id:012x}"),
        app_id: APP,
        hook_id: APP,
        installation_id: Some(INSTALL),
        repo_id: Some(10),
        event: "issue_comment".into(),
        action: Some("created".into()),
        url: "http://example.com/webhook".into(),
        delivered_at: Timestamp::SEED,
        redelivery: false,
        duration: 0.02,
        status: if status_code == 200 {
            "OK"
        } else {
            "Invalid HTTP Response: 503"
        }
        .into(),
        status_code,
        request: DeliveryRequest {
            headers: BTreeMap::new(),
            payload: json!({"action": "created", "comment": {"id": 20, "body": "/agent review"}})
                .to_string()
                .into(),
        },
        response: DeliveryResponse {
            headers: BTreeMap::new(),
            payload: None,
        },
    }
}

fn bench(c: &mut Criterion) {
    let mut builder = StoreBuilder::new();
    let mut git = GitBuilder::new(&mut builder);
    let mut rest = Registry::new();
    hubdouble_api_apps::register(&mut Registrar {
        store: &mut builder,
        rest: &mut rest,
        gql: &mut hubdouble_gql::Registry::new(),
        git: &mut git,
        admin: &mut Admin::new(),
    });
    let clock = Arc::new(ManualClock::new(Timestamp::SEED));
    git.clock(clock.clone());
    let git = git.build();
    let store = builder.build();
    let actor = prepare(&store).unwrap_or_else(|e| panic!("{e}"));
    let headers = HeaderMap::new();
    let body = Bytes::new();
    let urls = store
        .view(|tx| Ok(Urls::new(tx.config(), "")))
        .unwrap_or_else(|e| panic!("{e}"));

    for (path, query) in [
        ("/app/installations", "per_page=100"),
        ("/app/hook/deliveries", "per_page=100&status=failure"),
        ("/app/hook/deliveries/1000", ""),
    ] {
        let route = rest
            .route(&Method::GET, path)
            .unwrap_or_else(|| panic!("{path}"));
        let entry = route.entry.unwrap_or_else(|| panic!("{path}"));
        let query = Query::parse(query);
        c.bench_function(path, |b| {
            b.iter(|| {
                store
                    .view(|tx| {
                        let mut ctx = Ctx::new(Parts {
                            operation_id: entry.operation_id,
                            path,
                            params: &route.params,
                            query: &query,
                            headers: &headers,
                            body: &body,
                            actor: black_box(&actor),
                            repo: None,
                            git: &git,
                            access: Access::Read(tx),
                            clock: clock.as_ref(),
                            urls: &urls,
                        });
                        (entry.handler)(&mut ctx)
                    })
                    .unwrap_or_else(|e| panic!("{e}"))
            });
        });
    }
}

criterion_group!(benches, bench);
criterion_main!(benches);
