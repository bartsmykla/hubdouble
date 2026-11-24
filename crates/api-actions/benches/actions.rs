use std::collections::BTreeMap;
use std::hint::black_box;
use std::sync::Arc;

use bytes::Bytes;
use criterion::{Criterion, criterion_group, criterion_main};
use http::{HeaderMap, Method};
use hubdouble_admin::{Admin, Registrar};
use hubdouble_auth::Actor;
use hubdouble_git::{GitBuilder, ReadJobs};
use hubdouble_model::{
    Account, AccountKind, ApiError, ManualClock, Repo, RunStatus, Timestamp, Visibility,
    WorkflowRun, node_id,
};
use hubdouble_render::Urls;
use hubdouble_rest::{Access, Ctx, Parts, Query, Registry};
use hubdouble_store::{Accounts, Repos, Store, StoreBuilder, WorkflowRuns};

const PATH: &str = "/repos/demo/project/actions/runs";
const SHA: &str = "0123456789012345678901234567890123456789";

fn seed(store: &Store) -> Result<Arc<Repo>, ApiError> {
    store.update(|tx| {
        let now = Timestamp::SEED;
        tx.table_mut::<Accounts>().insert(Account {
            id: 1,
            node_id: node_id::entity(node_id::USER, 1),
            login: "demo".into(),
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
        let repo = tx.table_mut::<Repos>().insert(Repo {
            id: 2,
            node_id: node_id::entity(node_id::REPOSITORY, 2),
            owner_id: 1,
            name: "project".into(),
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
            has_projects: true,
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
            created_at: now,
            updated_at: now,
            pushed_at: None,
        })?;
        let mut table = tx.table_mut::<WorkflowRuns>();
        for id in 100..1100 {
            table.insert(WorkflowRun {
                id,
                node_id: node_id::entity(node_id::WORKFLOW_RUN, id),
                repo_id: 2,
                workflow_id: 3,
                name: "CI".into(),
                path: ".github/workflows/ci.yml".into(),
                display_title: "CI".into(),
                run_number: id - 99,
                run_attempt: 1,
                event: "workflow_dispatch".into(),
                status: RunStatus::Queued,
                conclusion: None,
                head_branch: Some(if id % 5 == 0 {
                    "main".into()
                } else {
                    "feature".into()
                }),
                head_sha: SHA.into(),
                head_repo_id: 2,
                actor_id: 1,
                triggering_actor_id: 1,
                check_suite_id: Some(id + 1000),
                pull_request_numbers: Vec::new(),
                inputs: BTreeMap::new(),
                created_at: now,
                updated_at: now,
                run_started_at: None,
            })?;
        }
        Ok(repo)
    })
}

fn run_list(c: &mut Criterion) {
    let mut store = StoreBuilder::new();
    let mut git = GitBuilder::new(&mut store);
    let mut rest = Registry::new();
    hubdouble_api_actions::register(&mut Registrar {
        store: &mut store,
        rest: &mut rest,
        gql: &mut hubdouble_gql::Registry::new(),
        git: &mut git,
        admin: &mut Admin::new(),
    });
    let git = git.build();
    let store = store.build();
    let repo = seed(&store).unwrap_or_else(|e| panic!("{e}"));
    let route = rest
        .route(&Method::GET, PATH)
        .unwrap_or_else(|| panic!("Missing route"));
    let entry = route.entry.unwrap_or_else(|| panic!("Missing handler"));
    let urls = store
        .view(|tx| Ok(Urls::new(tx.config(), "")))
        .unwrap_or_else(|e| panic!("{e}"));
    let query = Query::parse("event=workflow_dispatch&branch=main&status=queued&per_page=30");
    let headers = HeaderMap::new();
    let body = Bytes::new();
    let clock = ManualClock::new(Timestamp::SEED);
    c.bench_function("actions/list 30 filtered runs of 1000", |bench| {
        bench.iter(|| {
            black_box(
                store
                    .view(|tx| {
                        (entry.handler)(&mut Ctx::new(Parts {
                            operation_id: entry.operation_id,
                            path: PATH,
                            params: &route.params,
                            query: &query,
                            headers: &headers,
                            body: &body,
                            actor: &Actor::Anonymous,
                            repo: Some(&repo),
                            git: &git,
                            read_jobs: &ReadJobs::default(),
                            access: Access::Read(tx),
                            clock: &clock,
                            urls: &urls,
                        }))
                    })
                    .unwrap_or_else(|e| panic!("{e}")),
            );
        });
    });
}

criterion_group!(benches, run_list);
criterion_main!(benches);
