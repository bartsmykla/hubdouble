#![expect(clippy::unwrap_used, reason = "a benchmark stops on a setup failure")]

use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use hubdouble_api_issues::{comment_view, issue_view};
use hubdouble_model::{
    Account, AccountKind, Config, Issue, IssueComment, IssueState, Repo, Timestamp, Visibility,
    node_id,
};
use hubdouble_render::Urls;
use hubdouble_store::{Accounts, IssueComments, Issues, Repos, StoreBuilder};

const OWNER: i64 = 1;
const REPO: i64 = 2;
const ISSUE: i64 = 3;
const COMMENTS: i64 = 30;

fn account(id: i64, login: &str) -> Account {
    Account {
        id,
        node_id: node_id::entity(node_id::USER, id),
        login: login.to_owned(),
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
        id: REPO,
        node_id: node_id::entity(node_id::REPOSITORY, REPO),
        owner_id: OWNER,
        name: "public".to_owned(),
        description: None,
        homepage: None,
        language: None,
        topics: Vec::new(),
        visibility: Visibility::Public,
        fork_of: None,
        default_branch: "main".to_owned(),
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

fn issue() -> Issue {
    Issue {
        id: ISSUE,
        node_id: node_id::entity(node_id::ISSUE, ISSUE),
        repo_id: REPO,
        number: 1,
        title: "an issue".to_owned(),
        body: Some("body".to_owned()),
        user_id: OWNER,
        state: IssueState::Open,
        state_reason: None,
        locked: false,
        active_lock_reason: None,
        label_ids: Vec::new(),
        assignee_ids: Vec::new(),
        closed_by: None,
        created_at: Timestamp::SEED,
        updated_at: Timestamp::SEED,
        closed_at: None,
        last_edited_at: None,
        pull: None,
    }
}

fn bench(c: &mut Criterion) {
    let store = StoreBuilder::new().build();
    store
        .update(|tx| {
            tx.table_mut::<Accounts>().insert(account(OWNER, "alice"))?;
            tx.table_mut::<Repos>().insert(repo())?;
            tx.table_mut::<Issues>().insert(issue())?;
            for n in 0..COMMENTS {
                let id = 100 + n;
                tx.table_mut::<IssueComments>().insert(IssueComment {
                    id,
                    node_id: node_id::entity(node_id::ISSUE_COMMENT, id),
                    repo_id: REPO,
                    issue_id: ISSUE,
                    user_id: OWNER,
                    body: format!("comment {n}"),
                    via_app_id: None,
                    created_at: Timestamp::SEED,
                    updated_at: Timestamp::SEED,
                    last_edited_at: None,
                })?;
            }
            Ok(())
        })
        .unwrap();
    let urls = Urls::new(&Config::default(), "");

    c.bench_function("issues_render_a_page_of_30_comments", |b| {
        b.iter(|| {
            store
                .view(|tx| {
                    let repo = tx.table::<Repos>().get(REPO).unwrap();
                    let issue = tx.table::<Issues>().get(ISSUE).unwrap();
                    let page: Vec<_> = tx
                        .table::<IssueComments>()
                        .children(ISSUE)
                        .map(|c| comment_view(tx, &urls, repo, "alice/public", issue, c))
                        .collect();
                    Ok(serde_json::to_vec(black_box(&page)).unwrap())
                })
                .unwrap()
        });
    });
    c.bench_function("issues_render_one_issue", |b| {
        b.iter(|| {
            store
                .view(|tx| {
                    let repo = tx.table::<Repos>().get(REPO).unwrap();
                    let issue = tx.table::<Issues>().get(ISSUE).unwrap();
                    let v = issue_view(tx, &urls, repo, "alice/public", issue);
                    Ok(serde_json::to_vec(black_box(&v)).unwrap())
                })
                .unwrap()
        });
    });
}

criterion_group!(benches, bench);
criterion_main!(benches);
