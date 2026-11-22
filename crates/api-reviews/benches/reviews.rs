#![expect(clippy::unwrap_used, reason = "a benchmark stops on a setup failure")]

use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use http::{HeaderMap, Method};
use hubdouble_admin::{Admin, Registrar};
use hubdouble_auth::Actor;
use hubdouble_git::GitBuilder;
use hubdouble_model::{
    Account, AccountKind, Issue, IssueState, ManualClock, Pull, PullRef, Repo, Review, ReviewState,
    Timestamp, Visibility, node_id,
};
use hubdouble_render::Urls;
use hubdouble_rest::{Access, Ctx, Parts, Query, Registry};
use hubdouble_store::{Accounts, Issues, Repos, Reviews, StoreBuilder, TxMut};

const NOW: Timestamp = Timestamp::SEED;

fn populate(tx: &mut TxMut<'_>) -> Result<(), hubdouble_model::ApiError> {
    tx.table_mut::<Accounts>().insert(Account {
        id: 1,
        node_id: node_id::entity(node_id::USER, 1),
        login: "owner".to_owned(),
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
        created_at: NOW,
        updated_at: NOW,
    })?;
    tx.table_mut::<Repos>().insert(Repo {
        id: 2,
        node_id: node_id::entity(node_id::REPOSITORY, 2),
        owner_id: 1,
        name: "repo".to_owned(),
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
        allow_squash_merge: true,
        allow_rebase_merge: true,
        allow_auto_merge: false,
        delete_branch_on_merge: false,
        collaborators: Vec::new(),
        created_at: NOW,
        updated_at: NOW,
        pushed_at: None,
    })?;
    let base = PullRef {
        repo_id: Some(2),
        ref_name: "main".to_owned(),
        sha: "1".repeat(40),
    };
    tx.table_mut::<Issues>().insert(pull_issue(base))?;
    for id in 5..1005 {
        tx.table_mut::<Reviews>().insert(Review {
            id,
            node_id: node_id::entity(node_id::REVIEW, id),
            repo_id: 2,
            issue_id: 3,
            user_id: 1,
            body: "Please check this change".to_owned(),
            state: ReviewState::Commented,
            commit_id: Some("1".repeat(40)),
            submitted_at: Some(NOW),
            created_at: NOW,
            last_edited_at: None,
        })?;
    }
    Ok(())
}

fn pull_issue(base: PullRef) -> Issue {
    Issue {
        id: 3,
        node_id: node_id::entity(node_id::ISSUE, 3),
        repo_id: 2,
        number: 1,
        title: "Review the change".to_owned(),
        body: None,
        user_id: 1,
        state: IssueState::Open,
        state_reason: None,
        locked: false,
        active_lock_reason: None,
        label_ids: vec![],
        assignee_ids: vec![],
        closed_by: None,
        created_at: NOW,
        updated_at: NOW,
        closed_at: None,
        last_edited_at: None,
        pull: Some(Pull {
            id: 4,
            node_id: node_id::entity(node_id::PULL_REQUEST, 4),
            head: PullRef {
                ref_name: "topic".to_owned(),
                ..base.clone()
            },
            base,
            draft: false,
            merged_at: None,
            merge_commit_sha: None,
            merged_by: None,
            mergeable: None,
            maintainer_can_modify: true,
            requested_reviewers: vec![],
            requested_teams: vec![],
        }),
    }
}

fn bench(c: &mut Criterion) {
    let mut store = StoreBuilder::new();
    let mut git = GitBuilder::new(&mut store);
    let mut rest = Registry::new();
    hubdouble_api_reviews::register(&mut Registrar {
        store: &mut store,
        rest: &mut rest,
        gql: &mut hubdouble_gql::Registry::new(),
        git: &mut git,
        admin: &mut Admin::new(),
    });
    let store = store.build();
    let git = git.build();
    store.update(populate).unwrap();
    let path = "/repos/owner/repo/pulls/1/reviews";
    let route = rest.route(&Method::GET, path).unwrap();
    let handler = route.entry.unwrap().handler;
    let clock = ManualClock::new(NOW);
    let query = Query::parse("per_page=100&page=2");
    let headers = HeaderMap::new();
    let body = "".into();
    c.bench_function("list_100_reviews_from_1000", |b| {
        b.iter(|| {
            let reply = store
                .view(|tx| {
                    handler(&mut Ctx::new(Parts {
                        operation_id: route.operation_id,
                        path,
                        params: &route.params,
                        query: &query,
                        headers: &headers,
                        body: &body,
                        actor: &Actor::Anonymous,
                        repo: tx.table::<Repos>().get(2),
                        git: &git,
                        access: Access::Read(tx),
                        clock: &clock,
                        urls: &Urls::new(tx.config(), ""),
                    }))
                })
                .unwrap();
            black_box(reply);
        });
    });
}

criterion_group!(benches, bench);
criterion_main!(benches);
