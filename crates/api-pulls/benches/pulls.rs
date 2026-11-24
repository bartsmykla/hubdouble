use std::{collections::BTreeMap, hint::black_box, sync::Arc};

use criterion::{Criterion, criterion_group, criterion_main};
use http::{HeaderMap, Method};
use hubdouble_admin::{Admin, Registrar};
use hubdouble_auth::Actor;
use hubdouble_git::{FileChange, Git, GitBuilder, ReadJobs, Signature};
use hubdouble_model::{Account, AccountKind, ManualClock, Repo, Timestamp, Visibility, node_id};
use hubdouble_render::Urls;
use hubdouble_rest::{Access, Ctx, Parts, Query, Registry};
use hubdouble_store::{Accounts, Repos, Store, StoreBuilder};

fn account() -> Account {
    Account {
        id: 1,
        node_id: node_id::entity(node_id::USER, 1),
        login: "acme".into(),
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

fn seed_git(store: &Store, git: &Git) -> (Arc<Repo>, Arc<Account>, Urls) {
    store
        .update_quiet(|tx| {
            let account = tx.table_mut::<Accounts>().insert(account())?;
            let repo = tx.table_mut::<Repos>().insert(repo())?;
            let sig = Signature::seed();
            let base = git.commit(tx, repo.id, 1, "main", None, &[], "root", &sig, &sig, false)?;
            git.update_ref(
                tx,
                repo.id,
                1,
                "refs/heads/feature",
                None,
                Some(base),
                false,
            )?;
            git.commit(
                tx,
                repo.id,
                1,
                "feature",
                Some(base),
                &[FileChange::Write {
                    path: "feature.txt".into(),
                    mode: hubdouble_git::EntryMode::File,
                    data: b"content\n".to_vec().into(),
                }],
                "feature",
                &sig,
                &sig,
                false,
            )?;
            Ok((repo, account, Urls::new(tx.config(), "")))
        })
        .unwrap_or_else(|e| panic!("seed benchmark history: {e:?}"))
}

fn personal(user: Arc<Account>) -> Actor {
    Actor::User {
        user,
        token: Arc::new(hubdouble_model::Token {
            id: 3,
            token: String::new(),
            kind: hubdouble_model::TokenKind::Personal,
            app_id: None,
            installation_id: None,
            user_id: Some(1),
            repo_ids: None,
            permissions: BTreeMap::new(),
            scopes: vec!["repo".into()],
            expires_at: None,
            revoked: false,
            owner_seeded: false,
            created_at: Timestamp::SEED,
        }),
    }
}

fn bench(c: &mut Criterion) {
    let mut builder = StoreBuilder::new();
    let mut git = GitBuilder::new(&mut builder);
    let mut rest = Registry::new();
    hubdouble_api_pulls::register(&mut Registrar {
        store: &mut builder,
        git: &mut git,
        rest: &mut rest,
        gql: &mut hubdouble_gql::Registry::new(),
        admin: &mut Admin::new(),
    });
    let git = git.build();
    let store = builder.build();
    let clock = ManualClock::new(Timestamp::SEED);
    let (repo, account, urls) = seed_git(&store, &git);
    let actor = personal(account);
    let headers = HeaderMap::new();
    let body = "".into();
    let query = Query::default();
    let path = "/repos/acme/repo/pulls";
    let route = rest
        .route(&Method::POST, path)
        .unwrap_or_else(|| panic!("POST {path} is absent"));
    let create = route
        .entry
        .unwrap_or_else(|| panic!("POST {path} has no handler"))
        .handler;
    let input = r#"{"title":"feature","head":"feature","base":"main"}"#.into();
    store
        .update_quiet(|tx| {
            let read_jobs = ReadJobs::default();
            let mut ctx = Ctx::new(Parts {
                operation_id: route.operation_id,
                path,
                params: &route.params,
                query: &query,
                headers: &headers,
                body: &input,
                actor: &actor,
                repo: Some(&repo),
                git: &git,
                read_jobs: &read_jobs,
                access: Access::Write(tx.reborrow()),
                clock: &clock,
                urls: &urls,
            });
            create(&mut ctx)?;
            Ok(())
        })
        .unwrap_or_else(|e| panic!("open benchmark pull request: {e:?}"));
    for path in [
        path,
        "/repos/acme/repo/pulls/1",
        "/repos/acme/repo/pulls/1/commits",
        "/repos/acme/repo/pulls/1/files",
        "/repos/acme/repo/commits/feature/pulls",
    ] {
        let route = rest
            .route(&Method::GET, path)
            .unwrap_or_else(|| panic!("GET {path} is absent"));
        let handler = route
            .entry
            .unwrap_or_else(|| panic!("GET {path} has no handler"))
            .handler;
        c.bench_function(route.operation_id, |b| {
            b.iter(|| {
                store
                    .view(|tx| {
                        let read_jobs = ReadJobs::default();
                        let mut ctx = Ctx::new(Parts {
                            operation_id: route.operation_id,
                            path,
                            params: &route.params,
                            query: &query,
                            headers: &headers,
                            body: &body,
                            actor: &Actor::Anonymous,
                            repo: Some(&repo),
                            git: &git,
                            read_jobs: &read_jobs,
                            access: Access::Read(tx),
                            clock: &clock,
                            urls: &urls,
                        });
                        handler(black_box(&mut ctx))
                    })
                    .unwrap_or_else(|e| panic!("GET {path}: {e:?}"))
            });
        });
    }
}

criterion_group!(benches, bench);
criterion_main!(benches);
