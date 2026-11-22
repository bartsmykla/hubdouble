use std::hint::black_box;
use std::sync::Arc;

use bytes::Bytes;
use criterion::{Criterion, criterion_group, criterion_main};
use http::{HeaderMap, Method};
use hubdouble_admin::{Admin, Registrar};
use hubdouble_auth::Actor;
use hubdouble_git::GitBuilder;
use hubdouble_model::{
    Account, AccountKind, ApiError, ManualClock, Timestamp, Token, TokenKind, node_id,
};
use hubdouble_render::Urls;
use hubdouble_rest::{Access, Ctx, Parts, Query, Registry};
use hubdouble_store::{Accounts, Store, StoreBuilder, Tokens};

fn prepare(store: &Store) -> Result<Actor, ApiError> {
    let now = Timestamp::SEED;
    store.update(|tx| {
        let user = tx.table_mut::<Accounts>().insert(Account {
            id: 1,
            node_id: node_id::entity(node_id::USER, 1),
            login: "octocat".into(),
            kind: AccountKind::User,
            name: Some("The Octocat".into()),
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
        let token = tx.table_mut::<Tokens>().insert(Token {
            id: 2,
            token: "ghu_bench".into(),
            kind: TokenKind::User,
            app_id: None,
            installation_id: None,
            user_id: Some(1),
            repo_ids: None,
            permissions: std::collections::BTreeMap::new(),
            scopes: Vec::new(),
            expires_at: None,
            revoked: false,
            owner_seeded: false,
            created_at: now,
        })?;
        Ok(Actor::User { token, user })
    })
}

fn bench(c: &mut Criterion) {
    let mut builder = StoreBuilder::new();
    let mut git = GitBuilder::new(&mut builder);
    let mut rest = Registry::new();
    hubdouble_api_users::register(&mut Registrar {
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
    let query = Query::parse("");
    let urls = store
        .view(|tx| Ok(Urls::new(tx.config(), "")))
        .unwrap_or_else(|e| panic!("{e}"));

    for path in ["/user", "/users/octocat", "/user/1"] {
        let route = rest
            .route(&Method::GET, path)
            .unwrap_or_else(|| panic!("{path}"));
        let entry = route.entry.unwrap_or_else(|| panic!("{path}"));
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
