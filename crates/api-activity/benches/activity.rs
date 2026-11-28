use std::hint::black_box;
use std::sync::Arc;

use bytes::Bytes;
use criterion::{Criterion, criterion_group, criterion_main};
use http::{HeaderMap, Method};
use hubdouble_admin::{Admin, Registrar};
use hubdouble_auth::Actor;
use hubdouble_git::{GitBuilder, ReadJobs};
use hubdouble_model::{Account, AccountKind, ApiError, ManualClock, Timestamp, node_id};
use hubdouble_render::Urls;
use hubdouble_rest::{Access, Ctx, Parts, Query, Registry};
use hubdouble_store::{Accounts, ActivityEvent, ActivityEvents, Store, StoreBuilder};
use serde_json::json;

const EVENTS: i64 = 1_000;

fn prepare(store: &Store) -> Result<(), ApiError> {
    let now = Timestamp::SEED;
    store.update(|tx| {
        tx.table_mut::<Accounts>().insert(Account {
            id: 1,
            node_id: node_id::entity(node_id::USER, 1),
            login: "octocat".into(),
            kind: AccountKind::User,
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
        let mut events = tx.table_mut::<ActivityEvents>();
        for id in 0..EVENTS {
            events.insert(ActivityEvent {
                id: 100 + id,
                kind: "IssuesEvent",
                actor_id: 1,
                repo_id: 2,
                repo_name: "octocat/hello".into(),
                org_id: None,
                payload: json!({"action": "opened", "issue": {"number": id}}),
                public: id % 4 != 0,
                created_at: now,
            })?;
        }
        Ok(())
    })
}

fn bench(c: &mut Criterion) {
    let mut builder = StoreBuilder::new();
    let mut git = GitBuilder::new(&mut builder);
    let mut rest = Registry::new();
    hubdouble_api_activity::register(&mut Registrar {
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
    prepare(&store).unwrap_or_else(|e| panic!("{e}"));
    let headers = HeaderMap::new();
    let body = Bytes::new();
    let query = Query::parse("per_page=100");
    let urls = store
        .view(|tx| Ok(Urls::new(tx.config(), "")))
        .unwrap_or_else(|e| panic!("{e}"));
    let actor = Actor::Anonymous;

    for path in ["/events", "/users/octocat/events/public"] {
        let route = rest
            .route(&Method::GET, path)
            .unwrap_or_else(|| panic!("{path}"));
        let entry = route.entry.unwrap_or_else(|| panic!("{path}"));
        c.bench_function(path, |b| {
            b.iter(|| {
                store
                    .view(|tx| {
                        let read_jobs = ReadJobs::default();
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
                            read_jobs: &read_jobs,
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
