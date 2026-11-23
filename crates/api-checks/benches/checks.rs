use std::{collections::BTreeMap, hint::black_box, sync::Arc};

use criterion::{Criterion, criterion_group, criterion_main};
use http::{HeaderMap, Method};
use hubdouble_admin::{Admin, Registrar};
use hubdouble_api_checks::actions;
use hubdouble_auth::Actor;
use hubdouble_git::{Git, GitBuilder, Signature};
use hubdouble_model::{
    Access, Account, AccountKind, ApiError, App, Installation, ManualClock, Repo,
    RepositorySelection, RunStatus, Timestamp, Token, TokenKind, Visibility, WorkflowJob, node_id,
};
use hubdouble_render::Urls;
use hubdouble_rest::{Access as StoreAccess, Ctx, Parts, Query, Registry};
use hubdouble_store::{Accounts, Apps, Installations, Repos, Store, StoreBuilder};

fn account(id: i64, login: &str, kind: AccountKind) -> Account {
    Account {
        id,
        node_id: node_id::entity(node_id::USER, id),
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

fn repo() -> Repo {
    Repo {
        id: 2,
        node_id: node_id::entity(node_id::REPOSITORY, 2),
        owner_id: 1,
        name: "repo".into(),
        description: None,
        homepage: None,
        language: None,
        topics: Vec::new(),
        visibility: Visibility::Public,
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
        allow_squash_merge: false,
        allow_rebase_merge: false,
        allow_auto_merge: false,
        delete_branch_on_merge: false,
        collaborators: Vec::new(),
        created_at: Timestamp::SEED,
        updated_at: Timestamp::SEED,
        pushed_at: None,
    }
}

fn app() -> App {
    App {
        id: 3,
        node_id: node_id::entity(node_id::APP, 3),
        slug: "checker".into(),
        name: "Checker".into(),
        owner_id: 1,
        bot_id: 4,
        description: None,
        external_url: "https://example.com".into(),
        client_id: "Iv1.checker".into(),
        client_secret: String::new(),
        public_key_pem: String::new(),
        private_key_pem: None,
        webhook: None,
        permissions: BTreeMap::from([("checks".into(), Access::Write)]),
        events: Vec::new(),
        device_flow: false,
        created_at: Timestamp::SEED,
        updated_at: Timestamp::SEED,
    }
}

fn installation() -> Installation {
    Installation {
        id: 5,
        node_id: node_id::entity(node_id::INSTALLATION, 5),
        app_id: 3,
        account_id: 1,
        repository_selection: RepositorySelection::All,
        repo_ids: Vec::new(),
        permissions: BTreeMap::from([("checks".into(), Access::Write)]),
        events: Vec::new(),
        suspended_at: None,
        suspended_by: None,
        created_at: Timestamp::SEED,
        updated_at: Timestamp::SEED,
    }
}

struct Fixture {
    repo: Arc<Repo>,
    actor: Actor,
    urls: Urls,
    sha: String,
}

fn seed(store: &Store, git: &Git) -> Fixture {
    store
        .update_quiet(|tx| {
            tx.table_mut::<Accounts>()
                .insert(account(1, "acme", AccountKind::Organization))?;
            let bot =
                tx.table_mut::<Accounts>()
                    .insert(account(4, "checker[bot]", AccountKind::Bot))?;
            let app = tx.table_mut::<Apps>().insert(app())?;
            let installation = tx.table_mut::<Installations>().insert(installation())?;
            let repo = tx.table_mut::<Repos>().insert(repo())?;
            let sig = Signature::seed();
            let sha = git.commit(tx, repo.id, 1, "main", None, &[], "root", &sig, &sig, false)?;
            let actor = Actor::Installation {
                token: Arc::new(Token {
                    id: 6,
                    token: String::new(),
                    kind: TokenKind::Installation,
                    app_id: Some(app.id),
                    installation_id: Some(installation.id),
                    user_id: None,
                    repo_ids: None,
                    permissions: BTreeMap::new(),
                    scopes: Vec::new(),
                    expires_at: None,
                    revoked: false,
                    owner_seeded: false,
                    created_at: Timestamp::SEED,
                }),
                installation,
                app,
                bot,
            };
            Ok(Fixture {
                repo,
                actor,
                urls: Urls::new(tx.config(), ""),
                sha: sha.to_string(),
            })
        })
        .unwrap_or_else(|e| panic!("seed the benchmark: {e:?}"))
}

fn bench(c: &mut Criterion) {
    let mut builder = StoreBuilder::new();
    let mut git = GitBuilder::new(&mut builder);
    let mut rest = Registry::new();
    hubdouble_api_checks::register(&mut Registrar {
        store: &mut builder,
        git: &mut git,
        rest: &mut rest,
        gql: &mut hubdouble_gql::Registry::new(),
        admin: &mut Admin::new(),
    });
    let git = git.build();
    let store = builder.build();
    let clock = ManualClock::new(Timestamp::SEED);
    let fixture = seed(&store, &git);
    let headers = HeaderMap::new();
    let empty = "".into();
    let query = Query::default();
    let path = "/repos/acme/repo/check-runs";
    let route = rest
        .route(&Method::POST, path)
        .unwrap_or_else(|| panic!("POST {path} is absent"));
    let create = route
        .entry
        .unwrap_or_else(|| panic!("POST {path} has no handler"))
        .handler;
    for n in 0..20 {
        let body = format!(
            r#"{{"name":"check {}","head_sha":"{}","status":"completed","conclusion":"success","output":{{"title":"t","summary":"s"}}}}"#,
            n % 5,
            fixture.sha
        )
        .into();
        store
            .update_quiet(|tx| {
                let mut ctx = Ctx::new(Parts {
                    operation_id: route.operation_id,
                    path,
                    params: &route.params,
                    query: &query,
                    headers: &headers,
                    body: &body,
                    actor: &fixture.actor,
                    repo: Some(&fixture.repo),
                    git: &git,
                    access: StoreAccess::Write(tx.reborrow()),
                    clock: &clock,
                    urls: &fixture.urls,
                });
                create(&mut ctx)?;
                Ok(())
            })
            .unwrap_or_else(|e| panic!("create a benchmark check run: {e:?}"));
    }
    let latest = Query::parse("filter=latest&per_page=100");
    for path in [
        format!("/repos/acme/repo/commits/{}/check-runs", fixture.sha),
        format!("/repos/acme/repo/commits/{}/status", fixture.sha),
    ] {
        let route = rest
            .route(&Method::GET, &path)
            .unwrap_or_else(|| panic!("GET {path} is absent"));
        let handler = route
            .entry
            .unwrap_or_else(|| panic!("GET {path} has no handler"))
            .handler;
        c.bench_function(route.operation_id, |b| {
            b.iter(|| {
                store
                    .view(|tx| {
                        let mut ctx = Ctx::new(Parts {
                            operation_id: route.operation_id,
                            path: &path,
                            params: &route.params,
                            query: &latest,
                            headers: &headers,
                            body: &empty,
                            actor: &Actor::Anonymous,
                            repo: Some(&fixture.repo),
                            git: &git,
                            access: StoreAccess::Read(tx),
                            clock: &clock,
                            urls: &fixture.urls,
                        });
                        handler(black_box(&mut ctx))
                    })
                    .unwrap_or_else(|e| panic!("GET {path}: {e:?}"))
            });
        });
    }
    bench_actions_job(c, &store, &git, &fixture);
}

fn job(id: i64, repo_id: i64, sha: &str) -> WorkflowJob {
    WorkflowJob {
        id,
        node_id: node_id::entity(node_id::CHECK_RUN, id),
        repo_id,
        run_id: 1,
        run_attempt: 1,
        name: format!("job {id}"),
        workflow_name: Some("CI".to_owned()),
        head_sha: sha.to_owned(),
        head_branch: Some("main".to_owned()),
        status: RunStatus::Queued,
        conclusion: None,
        steps: Vec::new(),
        labels: Vec::new(),
        runner_id: None,
        runner_name: None,
        check_run_id: Some(id),
        log_blob_id: None,
        created_at: Timestamp::SEED,
        started_at: None,
        completed_at: None,
    }
}

const SUITE_JOBS: i64 = 1000;

fn bench_actions_job(c: &mut Criterion, store: &Store, git: &Git, fixture: &Fixture) {
    let suite = store
        .update_quiet(|tx| {
            let suite = tx.next_id();
            actions::open_suite(
                tx,
                actions::Suite {
                    id: suite,
                    repo_id: fixture.repo.id,
                    head_branch: Some("main".to_owned()),
                    head_sha: fixture.sha.clone(),
                    created_at: Timestamp::SEED,
                },
            )?;
            for _ in 0..SUITE_JOBS {
                let id = tx.next_id();
                let job = job(id, fixture.repo.id, &fixture.sha);
                actions::write_job(tx, git, (&job, suite), Vec::new(), 1)?;
            }
            Ok(suite)
        })
        .unwrap_or_else(|e| panic!("seed the Actions suite: {e:?}"));
    c.bench_function("actions job check run in a suite of 1000", |b| {
        b.iter(|| {
            let rolled_back = store.update_quiet(|tx| {
                let id = tx.next_id();
                let job = job(id, fixture.repo.id, &fixture.sha);
                actions::write_job(tx, git, (&job, suite), Vec::new(), 1)?;
                Err::<(), _>(ApiError::conflict("roll back"))
            });
            black_box(rolled_back)
        });
    });
}

criterion_group!(benches, bench);
criterion_main!(benches);
