use std::collections::BTreeSet;

use bytes::Bytes;
use http::HeaderMap;
use http::header::LINK;
use hubdouble_auth::Actor;
use hubdouble_git::{GitBuilder, ReadJobs};
use hubdouble_model::{
    Account, AccountKind, ManualClock, Repo, SquashMergeCommitMessage, SquashMergeCommitTitle,
    Timestamp, Visibility, node_id,
};
use hubdouble_render::Urls;
use hubdouble_store::{Accounts, Repos, StoreBuilder};
use serde_json::Value;

use crate::{Access, Ctx, Params, Parts, Query, selected_repositories};

fn org() -> Account {
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

fn repo(id: i64) -> Repo {
    Repo {
        id,
        node_id: node_id::entity(node_id::REPOSITORY, id),
        owner_id: 1,
        name: format!("repo-{id}"),
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
        squash_merge_commit_title: SquashMergeCommitTitle::CommitOrPrTitle,
        squash_merge_commit_message: SquashMergeCommitMessage::CommitMessages,
        allow_auto_merge: false,
        delete_branch_on_merge: false,
        collaborators: Vec::new(),
        created_at: Timestamp::SEED,
        updated_at: Timestamp::SEED,
        pushed_at: None,
    }
}

/// The answer to `ids` for `query`, with repositories 10 to 14 in the store.
fn answer(ids: &[i64], query: &str) -> (Value, Option<String>) {
    let mut builder = StoreBuilder::new();
    let git = GitBuilder::new(&mut builder).build();
    let store = builder.build();
    store
        .update(|tx| {
            tx.table_mut::<Accounts>().insert(org())?;
            for id in 10..15 {
                tx.table_mut::<Repos>().insert(repo(id))?;
            }
            Ok(())
        })
        .unwrap();
    let ids: BTreeSet<i64> = ids.iter().copied().collect();
    let reply = store
        .view(|tx| {
            let urls = Urls::new(tx.config(), "");
            let (params, query) = (Params::new(Vec::new()), Query::parse(query));
            let (headers, body, read_jobs) = (HeaderMap::new(), Bytes::new(), ReadJobs::default());
            let clock = ManualClock::new(Timestamp::SEED);
            let ctx = Ctx::new(Parts {
                operation_id: "actions/list-selected-repos-for-org-secret",
                path: "/orgs/acme/actions/secrets/TOKEN/repositories",
                params: &params,
                query: &query,
                headers: &headers,
                body: &body,
                actor: &Actor::Anonymous,
                repo: None,
                git: &git,
                read_jobs: &read_jobs,
                access: Access::Read(tx),
                clock: &clock,
                urls: &urls,
            });
            selected_repositories(&ctx, &ids)
        })
        .unwrap();
    let link = reply
        .headers
        .get(LINK)
        .map(|v| v.to_str().unwrap().to_owned());
    (serde_json::from_slice(&reply.body).unwrap(), link)
}

fn ids(page: &Value) -> Vec<i64> {
    page["repositories"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_i64().unwrap())
        .collect()
}

#[test]
fn the_list_counts_existing_repositories_and_pages_them_in_id_order() {
    // Given a selection of four repositories, one of them deleted, read two per page
    let selection = [13, 99, 10, 12];

    // When the second page and the first page are read
    let (second, second_link) = answer(&selection, "per_page=2&page=2");
    let (first, first_link) = answer(&selection, "per_page=2");

    // Then the total counts the three that exist, each page holds its share, and Link links them
    assert_eq!(first["total_count"], 3);
    assert_eq!(ids(&first), [10, 12]);
    assert_eq!(second["total_count"], 3);
    assert_eq!(ids(&second), [13]);
    assert_eq!(second["repositories"][0]["full_name"], "acme/repo-13");
    assert!(first_link.is_some_and(|l| l.contains("rel=\"next\"")));
    assert!(second_link.is_some_and(|l| l.contains("rel=\"prev\"")));
}

#[test]
fn an_empty_selection_is_an_empty_list_without_link() {
    let (page, link) = answer(&[], "");
    assert_eq!(page["total_count"], 0);
    assert_eq!(ids(&page), [0_i64; 0]);
    assert_eq!(link, None);
}
