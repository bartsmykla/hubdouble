use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use hubdouble_admin::{Admin, Registrar};
use hubdouble_auth::Actor;
use hubdouble_git::GitBuilder;
use hubdouble_gql::{Registry, Request, execute};
use hubdouble_model::{
    Account, AccountKind, Issue, IssueState, ManualClock, Pull, PullRef, Repo, Timestamp,
    Visibility, node_id,
};
use hubdouble_render::Urls;
use hubdouble_store::{Accounts, Issues, Repos, StoreBuilder};
use serde_json::json;

const QUERY: &str = "query($query: String!, $first: Int!) {
  search(query: $query, type: ISSUE, first: $first) { issueCount nodes { __typename } }
}";

fn account() -> Account {
    Account {
        id: 1,
        node_id: node_id::entity(node_id::ORGANIZATION, 1),
        login: "acme".into(),
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

fn issue(number: i64) -> Issue {
    let id = 100 + number;
    let pull = (number % 2 == 0).then(|| Pull {
        id: 10_000 + number,
        node_id: node_id::entity(node_id::PULL_REQUEST, 10_000 + number),
        head: PullRef {
            repo_id: Some(2),
            ref_name: if number % 4 == 0 {
                format!("prompts/proposal-{number}")
            } else {
                format!("feature-{number}")
            },
            sha: "0".repeat(40),
        },
        base: PullRef {
            repo_id: Some(2),
            ref_name: "main".into(),
            sha: "0".repeat(40),
        },
        draft: false,
        merged_at: None,
        merge_commit_sha: None,
        merged_by: None,
        mergeable: None,
        rebaseable: None,
        maintainer_can_modify: false,
        requested_reviewers: Vec::new(),
        requested_teams: Vec::new(),
    });
    Issue {
        id,
        node_id: node_id::entity(node_id::ISSUE, id),
        repo_id: 2,
        number,
        title: format!("Item {number}"),
        body: None,
        user_id: 1,
        state: IssueState::Open,
        state_reason: None,
        locked: false,
        active_lock_reason: None,
        label_ids: Vec::new(),
        assignee_ids: Vec::new(),
        closed_by: None,
        created_at: Timestamp::SEED,
        updated_at: Timestamp::SEED.plus_seconds(number),
        closed_at: None,
        last_edited_at: None,
        pull,
    }
}

fn bench(c: &mut Criterion) {
    let mut builder = StoreBuilder::new();
    let mut git = GitBuilder::new(&mut builder);
    let mut gql = Registry::new();
    let mut r = Registrar {
        store: &mut builder,
        git: &mut git,
        rest: &mut hubdouble_rest::Registry::new(),
        gql: &mut gql,
        admin: &mut Admin::new(),
    };
    hubdouble_api_search::register(&mut r);
    r.gql.object::<Issue>("PullRequest");
    r.gql.object::<Issue>("Issue");
    let git = git.build();
    let store = builder.build();
    let urls = store
        .update_quiet(|tx| {
            tx.table_mut::<Accounts>().insert(account())?;
            tx.table_mut::<Repos>().insert(repo())?;
            for number in 1..=1000 {
                tx.table_mut::<Issues>().insert(issue(number))?;
            }
            Ok(Urls::new(tx.config(), ""))
        })
        .unwrap_or_else(|e| panic!("seed the benchmark issues: {e:?}"));
    let clock = ManualClock::new(Timestamp::SEED);
    let variables = json!({
        "query": "repo:acme/repo is:pr head:prompts/proposal- sort:updated-desc",
        "first": 50,
    });
    let variables =
        hubdouble_gql::Variables::from(variables.as_object().cloned().unwrap_or_default());
    c.bench_function("search proposals of 1000 items", |b| {
        b.iter(|| {
            let answer = execute(
                &gql,
                &store,
                &git,
                &Actor::Anonymous,
                Request {
                    query: QUERY,
                    operation_name: None,
                    variables: &variables,
                    clock: &clock,
                    urls: &urls,
                    request_id: "",
                },
            );
            black_box(answer)
        });
    });
}

criterion_group!(benches, bench);
criterion_main!(benches);
