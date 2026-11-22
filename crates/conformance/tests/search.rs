use hubdouble_admin::Registrar;
use hubdouble_model::{
    Access, ApiError, Issue, IssueState, Label, Pull, PullRef, Timestamp, Token, TokenKind, node_id,
};
use hubdouble_store::{Issues, Labels, Tokens, TxMut};
use hubdouble_testkit::{GoGithub, Graphql, TestServer, fixture};
use serde_json::{Value, json};

const DAVE_TOKEN: &str = "ghp_da4e0000000000000000000000000000000";
const INSTALLATION_TOKEN: &str = "ghs_search_conformance";

const SEARCH: &str = "query($query: String!, $first: Int!, $after: String) {
  search(query: $query, type: ISSUE, first: $first, after: $after) {
    issueCount
    pageInfo { hasNextPage endCursor }
    nodes { __typename ... on PullRequest { title } ... on Issue { title } }
  }
}";

struct Item {
    repo: i64,
    number: i64,
    title: &'static str,
    body: &'static str,
    author: i64,
    closed: bool,
    labels: &'static [&'static str],
    branches: Option<(&'static str, &'static str)>,
    draft: bool,
    merged: bool,
}

const PUBLIC: i64 = fixture::PUBLIC_REPO_ID;
const PRIVATE: i64 = fixture::PRIVATE_REPO_ID;

const ITEMS: &[Item] = &[
    Item {
        repo: PUBLIC,
        number: 1,
        title: "Proposal alpha",
        body: "",
        author: fixture::BOB_ID,
        closed: false,
        labels: &[],
        branches: Some(("prompts/proposal-alpha", "main")),
        draft: false,
        merged: false,
    },
    Item {
        repo: PUBLIC,
        number: 2,
        title: "Proposal beta",
        body: "",
        author: fixture::ALICE_ID,
        closed: false,
        labels: &[],
        branches: Some(("prompts/proposal-beta", "main")),
        draft: true,
        merged: false,
    },
    Item {
        repo: PUBLIC,
        number: 3,
        title: "Fix the parser",
        body: "",
        author: fixture::BOB_ID,
        closed: true,
        labels: &["bug"],
        branches: Some(("fix-parser", "develop")),
        draft: false,
        merged: true,
    },
    Item {
        repo: PUBLIC,
        number: 4,
        title: "Parser crashes",
        body: "It fails on quotes",
        author: fixture::CAROL_ID,
        closed: false,
        labels: &["Bug", "good first issue"],
        branches: None,
        draft: false,
        merged: false,
    },
    Item {
        repo: PUBLIC,
        number: 5,
        title: "Write docs",
        body: "Explain the PARSER",
        author: fixture::ALICE_ID,
        closed: true,
        labels: &["docs"],
        branches: None,
        draft: false,
        merged: false,
    },
    Item {
        repo: PUBLIC,
        number: 6,
        title: "Old proposal",
        body: "",
        author: fixture::BOB_ID,
        closed: true,
        labels: &[],
        branches: Some(("prompts/proposal-old", "main")),
        draft: false,
        merged: false,
    },
    Item {
        repo: PRIVATE,
        number: 1,
        title: "Private parser issue",
        body: "",
        author: fixture::ALICE_ID,
        closed: false,
        labels: &[],
        branches: None,
        draft: false,
        merged: false,
    },
    Item {
        repo: PRIVATE,
        number: 2,
        title: "Private proposal",
        body: "",
        author: fixture::ALICE_ID,
        closed: false,
        labels: &[],
        branches: Some(("prompts/proposal-secret", "main")),
        draft: false,
        merged: false,
    },
];

fn search_parents(r: &mut Registrar<'_>) {
    r.gql.object::<Issue>("PullRequest");
    r.gql.object::<Issue>("Issue");
}

fn label(tx: &mut TxMut<'_>, name: &str) -> Result<i64, ApiError> {
    let name = name.to_ascii_lowercase();
    if let Some(l) = tx.table::<Labels>().by_name(PUBLIC, &name) {
        return Ok(l.id);
    }
    let id = tx.next_id();
    tx.table_mut::<Labels>().insert(Label {
        id,
        node_id: node_id::entity(node_id::LABEL, id),
        repo_id: PUBLIC,
        name,
        color: "ededed".into(),
        description: None,
        default: false,
    })?;
    Ok(id)
}

fn insert(tx: &mut TxMut<'_>, age: i64, item: &Item) -> Result<(), ApiError> {
    let id = tx.next_id();
    let label_ids = item
        .labels
        .iter()
        .map(|name| label(tx, name))
        .collect::<Result<Vec<_>, _>>()?;
    let pull = match item.branches {
        Some((head, base)) => {
            let pull_id = tx.next_id();
            let branch = |name: &str| PullRef {
                repo_id: Some(item.repo),
                ref_name: name.into(),
                sha: "0".repeat(40),
            };
            Some(Pull {
                id: pull_id,
                node_id: node_id::entity(node_id::PULL_REQUEST, pull_id),
                head: branch(head),
                base: branch(base),
                draft: item.draft,
                merged_at: item.merged.then_some(Timestamp::SEED),
                merge_commit_sha: None,
                merged_by: None,
                mergeable: None,
                maintainer_can_modify: false,
                requested_reviewers: vec![],
                requested_teams: vec![],
            })
        }
        None => None,
    };
    tx.table_mut::<Issues>().insert(Issue {
        id,
        node_id: node_id::entity(node_id::ISSUE, id),
        repo_id: item.repo,
        number: item.number,
        title: item.title.into(),
        body: Some(item.body.into()).filter(|b: &String| !b.is_empty()),
        user_id: item.author,
        state: if item.closed {
            IssueState::Closed
        } else {
            IssueState::Open
        },
        state_reason: None,
        locked: false,
        active_lock_reason: None,
        label_ids,
        assignee_ids: vec![],
        closed_by: None,
        created_at: Timestamp::SEED.plus_seconds(age),
        updated_at: Timestamp::SEED.plus_seconds(1000 - age),
        closed_at: item.closed.then_some(Timestamp::SEED),
        last_edited_at: None,
        pull,
    })?;
    Ok(())
}

async fn server() -> TestServer {
    let server = TestServer::builder()
        .register(hubdouble_api_search::register)
        .register(search_parents)
        .fixture()
        .entry(json!({"kind": "user", "id": 1004, "login": "dave",
                      "tokens": [{"token": DAVE_TOKEN}]}))
        .start()
        .await;
    server
        .server
        .services()
        .store
        .update_quiet(|tx| {
            for (age, item) in (1..).zip(ITEMS) {
                insert(tx, age, item)?;
            }
            let id = tx.next_id();
            tx.table_mut::<Tokens>().insert(Token {
                id,
                token: INSTALLATION_TOKEN.into(),
                kind: TokenKind::Installation,
                app_id: Some(fixture::APP_ID),
                installation_id: Some(fixture::INSTALLATION_ID),
                user_id: None,
                repo_ids: None,
                permissions: [
                    ("issues".into(), Access::Read),
                    ("metadata".into(), Access::Read),
                ]
                .into(),
                scopes: vec![],
                expires_at: None,
                revoked: false,
                owner_seeded: false,
                created_at: Timestamp::SEED,
            })?;
            Ok(())
        })
        .unwrap_or_else(|e| panic!("seed the search records: {e}"));
    server
}

async fn search(server: &TestServer, token: &str, query: &str, after: Option<&str>) -> Value {
    let api = Graphql::new(GoGithub::new(server).with_token(token));
    let vars = json!({"query": query, "first": 50, "after": after});
    let body = api.query(SEARCH, &vars).await.json();
    assert_eq!(body.get("errors"), None, "{query}: {body}");
    body["data"]["search"].clone()
}

async fn titles(server: &TestServer, token: &str, query: &str) -> Vec<String> {
    let found = search(server, token, query, None).await;
    found["nodes"]
        .as_array()
        .unwrap_or_else(|| panic!("{query}: no nodes in {found}"))
        .iter()
        .map(|n| n["title"].as_str().unwrap_or_default().to_owned())
        .collect()
}

#[tokio::test]
async fn proposals_search_finds_open_and_closed_proposal_pulls_of_one_repository() {
    // Given pulls on proposal branches in two repositories, and other pulls and issues
    let server = server().await;

    // When client's proposals search runs (internal/proposals/proposals.go:385)
    let found = search(
        &server,
        fixture::ALICE_TOKEN,
        "repo:acme/public is:pr head:prompts/proposal- sort:updated-desc",
        None,
    )
    .await;

    // Then only that repository's proposal pulls come back, the latest update first
    assert_eq!(found["issueCount"], 3);
    assert_eq!(
        found["nodes"],
        json!([
            {"__typename": "PullRequest", "title": "Proposal alpha"},
            {"__typename": "PullRequest", "title": "Proposal beta"},
            {"__typename": "PullRequest", "title": "Old proposal"},
        ])
    );
}

#[tokio::test]
async fn every_qualifier_narrows_the_results() {
    // Given records that each match only some qualifiers
    let server = server().await;
    let cases: &[(&str, &[&str])] = &[
        (
            "repo:acme/public is:issue",
            &["Write docs", "Parser crashes"],
        ),
        (
            "repo:acme/public is:pr is:open",
            &["Proposal beta", "Proposal alpha"],
        ),
        (
            "repo:acme/public is:closed",
            &["Old proposal", "Write docs", "Fix the parser"],
        ),
        ("repo:acme/public is:merged", &["Fix the parser"]),
        ("repo:acme/public is:draft", &["Proposal beta"]),
        ("repo:acme/public base:develop", &["Fix the parser"]),
        (
            "repo:acme/public author:bob",
            &["Old proposal", "Fix the parser", "Proposal alpha"],
        ),
        (
            "repo:acme/public label:BUG",
            &["Parser crashes", "Fix the parser"],
        ),
        (
            r#"repo:acme/public label:bug label:"good first issue""#,
            &["Parser crashes"],
        ),
        (
            r#"repo:acme/public label:docs,"good first issue""#,
            &["Write docs", "Parser crashes"],
        ),
        (
            "repo:acme/public parser",
            &["Write docs", "Parser crashes", "Fix the parser"],
        ),
        (
            "repo:acme/public parser in:title",
            &["Parser crashes", "Fix the parser"],
        ),
        ("repo:acme/public parser in:body", &["Write docs"]),
        (
            "org:acme is:issue parser in:title",
            &["Private parser issue", "Parser crashes"],
        ),
        (
            "repo:acme/public is:pr head:prompts/proposal- sort:created-asc",
            &["Proposal alpha", "Proposal beta", "Old proposal"],
        ),
    ];
    for (query, expected) in cases {
        // When the query runs as an admin of every repository
        let found = titles(&server, fixture::ALICE_TOKEN, query).await;

        // Then exactly the matching records come back, in the expected order
        assert_eq!(found, *expected, "{query}");
    }
}

#[tokio::test]
async fn first_and_after_page_through_the_results() {
    // Given three proposal pulls
    let server = server().await;
    let query = "repo:acme/public is:pr head:prompts/proposal- sort:updated-desc";
    let api = Graphql::new(GoGithub::new(&server).with_token(fixture::ALICE_TOKEN));

    // When the first page holds one result, and the next page starts after it
    let first = api
        .query(SEARCH, &json!({"query": query, "first": 1}))
        .await
        .json()["data"]["search"]
        .clone();
    let cursor = first["pageInfo"]["endCursor"].as_str().unwrap_or_default();
    let next = search(&server, fixture::ALICE_TOKEN, query, Some(cursor)).await;

    // Then the count covers every result and the pages do not overlap
    assert_eq!(first["issueCount"], 3);
    assert_eq!(first["pageInfo"]["hasNextPage"], true);
    assert_eq!(first["nodes"][0]["title"], "Proposal alpha");
    assert_eq!(next["nodes"][0]["title"], "Proposal beta");
    assert_eq!(next["nodes"].as_array().map(Vec::len), Some(2));
}

#[tokio::test]
async fn hidden_repositories_never_appear() {
    // Given a user without a role on the private repository
    let server = server().await;

    // When that user searches the org and names the private repository
    let org = titles(&server, DAVE_TOKEN, "org:acme parser in:title").await;
    let named = search(&server, DAVE_TOKEN, "repo:acme/private", None).await;

    // Then only public records come back, and the private repository answers no results
    assert_eq!(org, ["Parser crashes", "Fix the parser"]);
    assert_eq!(named["issueCount"], 0);
    assert_eq!(named["nodes"], json!([]));
}

#[tokio::test]
async fn an_installation_sees_only_its_repositories_and_granted_kinds() {
    // Given an installation on the public repository only, with issues but not pull_requests
    let server = server().await;

    // When it searches the whole org
    let found = titles(&server, INSTALLATION_TOKEN, "org:acme parser in:title").await;

    // Then it finds the public issue, but neither the pull nor the private issue
    assert_eq!(found, ["Parser crashes"]);
}
