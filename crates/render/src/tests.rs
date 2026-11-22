use hubdouble_model::{self as m, Config, Timestamp, node_id};
use serde_json::{Value, json};

use crate::{Issue, Repository, SimpleUser, Urls};

fn urls() -> Urls {
    let config = Config {
        base_url: "http://hubdouble:8000".to_owned(),
        web_url: "https://github.com".to_owned(),
        ..Config::default()
    };
    Urls::new(&config, "/api/v3")
}

fn owner() -> m::Account {
    m::Account {
        id: 962_416,
        node_id: node_id::entity(node_id::ORGANIZATION, 962_416),
        login: "octo-org".to_owned(),
        kind: m::AccountKind::Organization,
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

fn repo() -> m::Repo {
    m::Repo {
        id: 7,
        node_id: node_id::entity(node_id::REPOSITORY, 7),
        owner_id: 962_416,
        name: "agent".to_owned(),
        description: None,
        homepage: None,
        language: None,
        topics: Vec::new(),
        visibility: m::Visibility::Private,
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
        allow_auto_merge: false,
        delete_branch_on_merge: false,
        collaborators: Vec::new(),
        created_at: Timestamp::SEED,
        updated_at: Timestamp::SEED,
        pushed_at: None,
    }
}

fn issue() -> m::Issue {
    m::Issue {
        id: 8,
        node_id: node_id::entity(node_id::ISSUE, 8),
        repo_id: 7,
        number: 1,
        title: "t".to_owned(),
        body: None,
        user_id: 962_416,
        state: m::IssueState::Open,
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

#[test]
fn empty_lists_render_as_arrays_and_missing_values_as_null() {
    let urls = urls();
    let r: Value = serde_json::to_value(Repository::new(&urls, &repo(), &owner())).unwrap();
    assert_eq!(r["topics"], json!([]));
    assert_eq!(r["description"], Value::Null);
    assert_eq!(r["pushed_at"], Value::Null);
    assert_eq!(r["private"], json!(true));
    assert_eq!(r["created_at"], json!("2026-01-01T00:00:00Z"));

    let user = SimpleUser::new(&urls, &owner());
    let i: Value = serde_json::to_value(Issue::new(
        &urls,
        &issue(),
        "octo-org/app",
        Some(user),
        Vec::new(),
        Vec::new(),
    ))
    .unwrap();
    assert_eq!(i["labels"], json!([]));
    assert_eq!(i["assignees"], json!([]));
    assert_eq!(i["assignee"], Value::Null);
    assert_eq!(i["body"], Value::Null);
    assert_eq!(i["milestone"], Value::Null);
    assert_eq!(i["closed_at"], Value::Null);
    assert!(i.get("pull_request").is_none());
    assert_eq!(i["state"], json!("open"));
}

#[test]
fn api_links_use_the_base_url_and_resource_links_use_the_web_url() {
    let urls = urls();
    let r: Value = serde_json::to_value(Repository::new(&urls, &repo(), &owner())).unwrap();
    assert_eq!(
        r["url"],
        json!("http://hubdouble:8000/api/v3/repos/octo-org/app")
    );
    assert_eq!(r["html_url"], json!("https://github.com/octo-org/app"));
    assert_eq!(r["owner"]["type"], json!("Organization"));
    assert_eq!(
        r["owner"]["url"],
        json!("http://hubdouble:8000/api/v3/users/octo-org")
    );
}
