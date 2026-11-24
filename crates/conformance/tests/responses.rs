#![expect(
    clippy::unwrap_used,
    reason = "tests fail immediately when an answer and its contract differ"
)]

use http::{HeaderMap, HeaderValue, StatusCode};
use hubdouble_testkit::fixture::{self, ALICE, ALICE_TOKEN};
use hubdouble_testkit::{GoGithub, Graphql, TestServer};
use serde_json::{Value, json};

const REPO: &str = "repos/acme/public";

async fn whole_server(extra: &[Value]) -> TestServer {
    let mut builder = TestServer::builder().frame(hubdouble_api::register_all);
    for entry in fixture::seed().into_iter().chain(extra.iter().cloned()) {
        builder = builder.entry(entry);
    }
    builder.start().await
}

async fn tip(client: &GoGithub, branch: &str) -> String {
    let answer = client.get(&format!("{REPO}/git/ref/heads/{branch}")).await;
    assert_eq!(answer.status, StatusCode::OK);
    answer.json()["object"]["sha"].as_str().unwrap().to_owned()
}

fn headers(pairs: &[(&'static str, &'static str)]) -> HeaderMap {
    let mut map = HeaderMap::new();
    for (name, value) in pairs {
        map.insert(*name, HeaderValue::from_static(value));
    }
    map
}

#[tokio::test]
async fn graphql_ignores_the_rest_api_version_and_answers_the_v4_media_type() {
    // Given
    let server = TestServer::builder()
        .frame(hubdouble_api::register_all)
        .fixture()
        .start()
        .await;
    let alice = Graphql::new(GoGithub::new(&server).with_token(ALICE_TOKEN));
    let query = "{ viewer { login } }";

    // When the client sends a version GitHub does not publish, as live gh api graphql did
    let unknown = alice
        .query_headers(
            query,
            &json!({}),
            headers(&[("x-github-api-version", "1999-01-01"), ("accept", "*/*")]),
        )
        .await;
    let plain_json = alice
        .query_headers(
            query,
            &json!({}),
            headers(&[("accept", "application/json")]),
        )
        .await;
    let go_github = alice.query(query, &json!({})).await;

    // Then GitHub answers 200 with the v4 media type, no selected version and its scope headers
    assert_eq!(unknown.status, StatusCode::OK);
    assert_eq!(unknown.json()["data"]["viewer"]["login"], ALICE);
    assert_eq!(
        unknown.header("x-github-media-type"),
        "github.v4; format=json"
    );
    assert!(
        !unknown
            .headers
            .contains_key("x-github-api-version-selected")
    );
    assert_eq!(unknown.header("x-ratelimit-resource"), "graphql");
    assert_eq!(unknown.header("x-accepted-oauth-scopes"), "repo");
    assert_ne!(unknown.header("x-oauth-scopes"), "");
    assert_ne!(unknown.header("x-github-request-id"), "");
    assert_eq!(plain_json.header("x-github-media-type"), "github.v4");
    assert_eq!(
        go_github.header("x-github-media-type"),
        "github.v3; format=json"
    );
    assert!(
        !go_github
            .headers
            .contains_key("x-github-api-version-selected")
    );
}

#[tokio::test]
async fn a_commit_status_is_validated_as_github_validates_a_status() {
    // Given
    let server = whole_server(&[]).await;
    let alice = GoGithub::new(&server).with_token(ALICE_TOKEN);
    let head = tip(&alice, "main").await;
    let path = format!("{REPO}/statuses/{head}");
    let longest: String = "é".repeat(140);
    let too_long: String = "a".repeat(141);

    // When the client writes statuses that GitHub refuses, as the samples in the plan show
    let long = alice
        .post(&path, &json!({"state": "success", "description": too_long}))
        .await;
    let relative = alice
        .post(
            &path,
            &json!({"state": "success", "target_url": "key-concepts/"}),
        )
        .await;
    let no_state = alice.post(&path, &json!({"context": "ci"})).await;
    let emoji = alice
        .post(&path, &json!({"state": "success", "context": "ci 🚀"}))
        .await;
    let listed = alice.get(&format!("{REPO}/commits/{head}/statuses")).await;
    let fits = alice
        .post(
            &path,
            &json!({"state": "success", "description": longest, "target_url": "HTTPS://ci.example/1"}),
        )
        .await;

    // Then each refusal is a 422 of the Status model and writes nothing
    for answer in [&long, &relative] {
        assert_eq!(answer.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(answer.json()["errors"].is_string(), "{}", answer.json());
    }
    for (answer, field) in [(&no_state, "state"), (&emoji, "context")] {
        assert_eq!(answer.status, StatusCode::UNPROCESSABLE_ENTITY);
        let error = &answer.json()["errors"][0];
        assert_eq!(error["resource"], "Status");
        assert_eq!(error["code"], "custom");
        assert_eq!(error["field"], field);
    }
    assert_eq!(listed.json(), json!([]));
    assert_eq!(fits.status, StatusCode::CREATED);
}

#[tokio::test]
async fn status_contexts_match_without_case_and_keep_their_spelling() {
    // Given a ruleset on main that requires the context CI/Build
    let rules = json!({"kind": "ruleset", "repository": "acme/public", "name": "rules",
        "target": "branch", "enforcement": "active", "bypass_actors": [],
        "conditions": {"ref_name": {"include": ["~DEFAULT_BRANCH"], "exclude": []}},
        "rules": [{"type": "required_status_checks", "parameters": {
            "strict_required_status_checks_policy": false,
            "required_status_checks": [{"context": "CI/Build"}]}}]});
    let server = whole_server(&[rules]).await;
    let alice = GoGithub::new(&server).with_token(ALICE_TOKEN);
    let head = tip(&alice, "feature").await;
    let path = format!("{REPO}/statuses/{head}");

    // When a pending CI/Build is followed by a successful ci/build on the head
    for (state, context) in [("pending", "CI/Build"), ("success", "ci/build")] {
        let made = alice
            .post(&path, &json!({"state": state, "context": context}))
            .await;
        assert_eq!(made.status, StatusCode::CREATED);
    }
    let combined = alice.get(&format!("{REPO}/commits/{head}/status")).await;
    let merged = alice
        .post(
            &format!("{REPO}/merges"),
            &json!({"base": "main", "head": "feature"}),
        )
        .await;

    // Then both are one context, shown as the newest status spelled it, and it passes the rule
    let combined = combined.json();
    assert_eq!(combined["total_count"], 1);
    assert_eq!(combined["state"], "success");
    assert_eq!(combined["statuses"][0]["context"], "ci/build");
    assert_eq!(merged.status, StatusCode::CREATED, "{}", merged.json());
}

const NO_SCOPE_PAT: &str = "ghp_0000000000000000000000000000000000c0";
const NO_SCOPE_OAUTH: &str = "gho_0000000000000000000000000000000000c1";

#[tokio::test]
async fn a_classic_token_without_scopes_reads_public_repositories_only() {
    // Given carol's classic personal and OAuth App tokens without scopes
    let pull = json!({"kind": "pull_request", "repository": "acme/public", "user": "alice",
        "title": "feature", "head": "feature", "base": "main"});
    let tokens: Vec<Value> = [NO_SCOPE_PAT, NO_SCOPE_OAUTH]
        .iter()
        .map(|t| json!({"kind": "token", "user": "carol", "token": t, "scopes": []}))
        .collect();
    let server = whole_server(&[vec![pull], tokens].concat()).await;
    let anonymous = GoGithub::new(&server);

    for token in [NO_SCOPE_PAT, NO_SCOPE_OAUTH] {
        let carol = GoGithub::new(&server).with_token(token);

        // When the token reads public reviews and runs, a private repository, and writes
        let reads = [
            "repos/acme/public/pulls/1/reviews",
            "repos/acme/public/actions/runs",
        ];
        let private = carol.get("repos/acme/private/actions/runs").await;
        let write = carol
            .post(
                "repos/acme/public/issues/1/comments",
                &json!({"body": "hi"}),
            )
            .await;

        // Then the public reads answer as for anyone, as GitHub's scope docs say
        for path in reads {
            assert_eq!(anonymous.get(path).await.status, StatusCode::OK, "{path}");
            assert_eq!(
                carol.get(path).await.status,
                StatusCode::OK,
                "{token} {path}"
            );
        }
        assert_eq!(private.status, StatusCode::NOT_FOUND, "{token}");
        assert_eq!(write.status, StatusCode::FORBIDDEN, "{token}");
        assert_eq!(
            write.header("x-accepted-oauth-scopes"),
            "repo, public_repo",
            "{token}"
        );
    }
}

#[test]
fn every_fallback_leaf_of_every_registered_type_is_checked_against_its_type() {
    // Given the registry of the whole server
    let mut store = hubdouble_store::StoreBuilder::new();
    let mut git = hubdouble_git::GitBuilder::new(&mut store);
    let mut gql = hubdouble_gql::Registry::new();
    hubdouble_api::register_all(&mut hubdouble_admin::Registrar {
        store: &mut store,
        rest: &mut hubdouble_rest::Registry::new(),
        gql: &mut gql,
        git: &mut git,
        admin: &mut hubdouble_admin::Admin::new(),
    });

    // When every leaf field that a type answers from its serde form is walked
    let unchecked = gql.unchecked_leaves();

    // Then the executor checks each against its scalar or enum type
    assert_eq!(unchecked, Vec::<String>::new());
}

#[tokio::test]
async fn graphql_leaves_answer_values_of_their_schema_types() {
    // Given a secret and a visible team, and a pull with a line and a file review comment
    let teams = [("Visible", "closed"), ("Hidden", "secret")].map(|(name, privacy)| {
        json!({"kind": "team", "org": "acme", "name": name, "privacy": privacy,
            "members": [{"login": "alice"}]})
    });
    let pull = json!({"kind": "pull_request", "repository": "acme/public", "user": "alice",
        "title": "feature", "head": "feature", "base": "main"});
    let server = whole_server(&[vec![pull], teams.to_vec()].concat()).await;
    let alice = GoGithub::new(&server).with_token(ALICE_TOKEN);
    let review = alice
        .post(
            &format!("{REPO}/pulls/1/reviews"),
            &json!({"event": "COMMENT", "body": "review", "comments": [
                {"path": "src/feature.txt", "line": 1, "body": "line"},
                {"path": "src/feature.txt", "subject_type": "file", "body": "file"}]}),
        )
        .await;
    assert_eq!(review.status, StatusCode::OK, "{}", review.json());

    // When the client reads the team privacy, the review ID, the lock reason and the positions
    let answer = Graphql::new(alice)
        .query(
            "{ organization(login: \"acme\") { teams(first: 10) { nodes { name privacy } } }
               repository(owner: \"acme\", name: \"public\") { pullRequest(number: 1) {
                 activeLockReason
                 reviews(first: 1) { nodes { id } }
                 reviewThreads(first: 5) { nodes { comments(first: 1) {
                   nodes { body originalPosition } } } } } } }",
            &json!({}),
        )
        .await;

    // Then each value is one of its GraphQL type, and a file comment has GitHub's position 1
    let data = &answer.json()["data"];
    let privacy: Vec<_> = data["organization"]["teams"]["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|team| (team["name"].as_str().unwrap(), team["privacy"].clone()))
        .collect();
    assert!(
        privacy.contains(&("Visible", json!("VISIBLE"))),
        "{privacy:?}"
    );
    assert!(
        privacy.contains(&("Hidden", json!("SECRET"))),
        "{privacy:?}"
    );
    let pull = &data["repository"]["pullRequest"];
    assert_eq!(pull["activeLockReason"], Value::Null);
    assert_eq!(pull["reviews"]["nodes"][0]["id"], review.json()["node_id"]);
    assert!(
        review.json()["node_id"]
            .as_str()
            .unwrap()
            .starts_with("PRR_")
    );
    let positions: Vec<_> = pull["reviewThreads"]["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|thread| thread["comments"]["nodes"][0]["originalPosition"].clone())
        .collect();
    assert_eq!(positions, [json!(1), json!(1)]);
    assert_eq!(answer.json().get("errors"), None, "{}", answer.json());
}
