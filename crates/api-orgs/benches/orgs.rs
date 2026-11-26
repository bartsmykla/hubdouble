use std::hint::black_box;
use std::sync::Arc;

use bytes::Bytes;
use criterion::{Criterion, criterion_group, criterion_main};
use http::{HeaderMap, Method};
use hubdouble_admin::{Admin, Registrar};
use hubdouble_auth::Actor;
use hubdouble_git::{GitBuilder, ReadJobs};
use hubdouble_gql::{Args, Ctx as GqlCtx, GqlError, Obj, Request, Resolved, Target};
use hubdouble_model::{
    Account, AccountKind, ApiError, ManualClock, Membership, MembershipState, OrgRole, Repo,
    RepoRole, Team, TeamMember, TeamPrivacy, TeamRepo, TeamRole, Timestamp, Token, TokenKind,
    Visibility, node_id,
};
use hubdouble_render::Urls;
use hubdouble_rest::{Access, Ctx, Parts, Query, Registry};
use hubdouble_store::{Accounts, Memberships, Repos, Store, StoreBuilder, Teams};
use serde_json::{Value, json};

const TEAMS: &str = include_str!("../../../docs/inventory/graphql/teams.graphql");
const ACCESS: &str = include_str!("../../../docs/inventory/graphql/access.graphql");

fn account(id: i64, login: &str, kind: AccountKind, prefix: &str) -> Account {
    Account {
        id,
        node_id: node_id::entity(prefix, id),
        login: login.into(),
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

fn repository() -> Repo {
    Repo {
        id: 3,
        node_id: node_id::entity(node_id::REPOSITORY, 3),
        owner_id: 1,
        name: "demo".into(),
        description: None,
        homepage: None,
        language: None,
        topics: Vec::new(),
        visibility: Visibility::Private,
        fork_of: None,
        default_branch: "main".into(),
        archived: false,
        disabled: false,
        is_template: false,
        has_issues: true,
        has_projects: false,
        has_wiki: false,
        has_discussions: false,
        allow_merge_commit: true,
        allow_squash_merge: true,
        allow_rebase_merge: true,
        squash_merge_commit_title: hubdouble_model::SquashMergeCommitTitle::CommitOrPrTitle,
        squash_merge_commit_message: hubdouble_model::SquashMergeCommitMessage::CommitMessages,
        allow_auto_merge: false,
        delete_branch_on_merge: false,
        collaborators: Vec::new(),
        created_at: Timestamp::SEED,
        updated_at: Timestamp::SEED,
        pushed_at: None,
    }
}

fn prepare(store: &Store) -> Result<(Actor, Arc<Repo>), ApiError> {
    store.update(|tx| {
        tx.table_mut::<Accounts>().insert(account(
            1,
            "acme",
            AccountKind::Organization,
            node_id::ORGANIZATION,
        ))?;
        let user = tx.table_mut::<Accounts>().insert(account(
            2,
            "bob",
            AccountKind::User,
            node_id::USER,
        ))?;
        let repo = tx.table_mut::<Repos>().insert(repository())?;
        tx.table_mut::<Memberships>().insert(Membership {
            id: 4,
            org_id: 1,
            user_id: 2,
            role: OrgRole::Member,
            state: MembershipState::Active,
        })?;
        for id in 5..=104 {
            tx.table_mut::<Teams>().insert(Team {
                id,
                node_id: node_id::entity(node_id::TEAM, id),
                org_id: 1,
                name: format!("Team {id}"),
                slug: format!("team-{id}"),
                description: None,
                privacy: TeamPrivacy::Closed,
                notifications_enabled: true,
                parent_id: (id > 5).then_some(5),
                members: if id == 6 {
                    vec![TeamMember {
                        user_id: 2,
                        role: TeamRole::Member,
                    }]
                } else {
                    Vec::new()
                },
                repos: if id == 5 {
                    vec![TeamRepo {
                        repo_id: 3,
                        role: RepoRole::Write,
                    }]
                } else {
                    Vec::new()
                },
                created_at: Timestamp::SEED,
                updated_at: Timestamp::SEED,
            })?;
        }
        let token = Arc::new(Token {
            id: 105,
            token: "benchmark".into(),
            kind: TokenKind::Personal,
            user_id: Some(2),
            app_id: None,
            installation_id: None,
            permissions: std::collections::BTreeMap::new(),
            repo_ids: None,
            scopes: vec!["repo".into(), "read:org".into()],
            expires_at: None,
            revoked: false,
            owner_seeded: false,
            created_at: Timestamp::SEED,
        });
        Ok((Actor::User { token, user }, repo))
    })
}

fn bench(c: &mut Criterion) {
    let mut builder = StoreBuilder::new();
    let mut git = GitBuilder::new(&mut builder);
    let mut rest = Registry::new();
    let mut gql = hubdouble_gql::Registry::new();
    hubdouble_api_orgs::register(&mut Registrar {
        store: &mut builder,
        rest: &mut rest,
        gql: &mut gql,
        git: &mut git,
        admin: &mut Admin::new(),
    });
    gql.field("Query", "repository", gql_repository);
    gql.object::<Repo>("Repository");
    gql.object::<Account>("User");
    gql.field("User", "databaseId", gql_user_id);
    let clock = ManualClock::new(Timestamp::SEED);
    let git = git.build();
    let store = builder.build();
    let (actor, repo) = prepare(&store).unwrap_or_else(|e| panic!("{e}"));
    let urls = store
        .view(|tx| Ok(Urls::new(tx.config(), "")))
        .unwrap_or_else(|e| panic!("{e}"));
    let mut headers = HeaderMap::new();
    headers.insert(
        http::header::ACCEPT,
        http::HeaderValue::from_static("application/vnd.github.v3.repository+json"),
    );
    let body = Bytes::new();
    for (path, query) in [
        ("/orgs/acme/members/bob", ""),
        ("/orgs/acme/teams", "per_page=100"),
        ("/orgs/acme/teams/team-5", ""),
        ("/orgs/acme/teams/team-5/memberships/bob", ""),
        ("/orgs/acme/teams/team-6/repos/acme/demo", ""),
        ("/repos/acme/demo/collaborators/bob/permission", ""),
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
                        let read_jobs = ReadJobs::default();
                        let mut ctx = Ctx::new(Parts {
                            operation_id: entry.operation_id,
                            path,
                            params: &route.params,
                            query: &query,
                            headers: &headers,
                            body: &body,
                            actor: black_box(&actor),
                            repo: Some(&repo),
                            git: &git,
                            read_jobs: &read_jobs,
                            access: Access::Read(tx),
                            clock: &clock,
                            urls: &urls,
                        });
                        (entry.handler)(&mut ctx)
                    })
                    .unwrap_or_else(|e| panic!("{e}"))
            });
        });
    }

    let vars = json!({"org": "acme", "login": "bob", "owner": "acme", "n0": "demo", "n1": "demo"});
    let vars = &hubdouble_gql::Variables::from(
        vars.as_object()
            .cloned()
            .unwrap_or_else(|| panic!("invalid GraphQL variables")),
    );
    for (name, query) in [("graphql/teams", TEAMS), ("graphql/access", ACCESS)] {
        c.bench_function(name, |b| {
            b.iter(|| {
                let result = hubdouble_gql::execute(
                    &gql,
                    &store,
                    &git,
                    &actor,
                    Request {
                        query,
                        operation_name: None,
                        variables: vars,
                        clock: &clock,
                        urls: &urls,
                        request_id: "",
                    },
                )
                .unwrap_or_else(|e| panic!("{e}"));
                assert!(result.errors.is_empty(), "{:?}", result.errors);
                result
            });
        });
    }
}

fn gql_repository(ctx: &GqlCtx<'_>, _: &Obj, args: &Args) -> Result<Resolved, GqlError> {
    let read = |name: &str| args.0.get(name).and_then(Value::as_str).unwrap_or_default();
    ctx.load(Target::Repo {
        owner: read("owner"),
        name: read("name"),
    })
}

fn gql_user_id(_: &GqlCtx<'_>, obj: &Obj, _: &Args) -> Result<Resolved, GqlError> {
    Ok(Resolved::Leaf(json!(obj.get::<Account>()?.id)))
}

criterion_group!(benches, bench);
criterion_main!(benches);
