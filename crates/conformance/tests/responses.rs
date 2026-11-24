#![expect(
    clippy::unwrap_used,
    reason = "tests fail immediately when an answer and its contract differ"
)]

use http::{HeaderMap, HeaderValue, Method, StatusCode};
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

#[tokio::test]
async fn a_sha_and_context_take_at_most_1000_statuses() {
    // Given 999 statuses of the context ci on the tip of main
    let seeded: Vec<Value> = (0..999)
        .map(|_| {
            json!({"kind": "status", "repository": "acme/public", "sha": "main",
                "creator": "alice", "state": "pending", "context": "ci"})
        })
        .collect();
    let server = whole_server(&seeded).await;
    let alice = GoGithub::new(&server).with_token(ALICE_TOKEN);
    let head = tip(&alice, "main").await;
    let path = format!("{REPO}/statuses/{head}");

    // When the client writes the 1000th, a 1001st spelled CI, and one of another context
    let last = alice
        .post(&path, &json!({"state": "success", "context": "ci"}))
        .await;
    let past = alice
        .post(&path, &json!({"state": "failure", "context": "CI"}))
        .await;
    let other = alice
        .post(&path, &json!({"state": "success", "context": "lint"}))
        .await;
    let combined = alice.get(&format!("{REPO}/commits/{head}/status")).await;

    // Then the 1001st is GitHub's validation error and does not change the combined state
    assert_eq!(last.status, StatusCode::CREATED);
    assert_eq!(past.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        past.json(),
        json!({"message": "Validation Failed",
            "errors": "Validation failed: This SHA and context has reached the maximum number of statuses.",
            "documentation_url": "https://docs.github.com/rest/commits/statuses#create-a-commit-status",
            "status": "422"})
    );
    assert_eq!(other.status, StatusCode::CREATED);
    assert_eq!(combined.json()["state"], "success");
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

#[tokio::test]
async fn an_issue_locks_with_each_reason_and_unlocks_as_github_does() {
    // Given an issue of acme/public, alice who pushes to it, and carol who reads it
    let server = whole_server(&[]).await;
    let alice = GoGithub::new(&server).with_token(ALICE_TOKEN);
    let carol = GoGithub::new(&server).with_token(fixture::CAROL_TOKEN);
    let opened = alice
        .post(&format!("{REPO}/issues"), &json!({"title": "talk"}))
        .await;
    assert_eq!(opened.status, StatusCode::CREATED);
    let issue = format!("{REPO}/issues/{}", opened.json()["number"]);
    let lock = format!("{issue}/lock");
    let query = r#"query($n: Int!) { repository(owner: "acme", name: "public") {
        issue(number: $n) { activeLockReason } } }"#;
    let vars = json!({"n": opened.json()["number"]});
    let graphql = Graphql::new(alice.clone());
    let mut actions = Vec::new();

    for (reason, value) in [
        (Some("off-topic"), "OFF_TOPIC"),
        (Some("too heated"), "TOO_HEATED"),
        (Some("resolved"), "RESOLVED"),
        (Some("spam"), "SPAM"),
        (None, ""),
    ] {
        // When alice locks it with the reason, or with no body, and unlocks it
        let locked = match reason {
            Some(r) => {
                alice
                    .call_json(Method::PUT, &lock, &json!({"lock_reason": r}))
                    .await
            }
            None => alice.call(Method::PUT, &lock).await,
        };
        let read = alice.get(&issue).await.json();
        let gql = graphql.query(query, &vars).await.json();
        let unlocked = alice.call(Method::DELETE, &lock).await;
        let after = alice.get(&issue).await.json();
        let gql_after = graphql.query(query, &vars).await.json();

        // Then REST and GraphQL answer the reason while it is locked, and none after
        assert_eq!(locked.status, StatusCode::NO_CONTENT, "{reason:?}");
        assert_eq!(read["locked"], true);
        assert_eq!(read["active_lock_reason"], json!(reason));
        let active = &gql["data"]["repository"]["issue"]["activeLockReason"];
        match reason {
            Some(_) => assert_eq!(active, value, "{gql}"),
            None => assert!(active.is_null(), "{gql}"),
        }
        assert_eq!(unlocked.status, StatusCode::NO_CONTENT);
        assert_eq!(after["locked"], false);
        assert!(after["active_lock_reason"].is_null());
        assert!(gql_after["data"]["repository"]["issue"]["activeLockReason"].is_null());
        actions.extend(["locked", "unlocked"]);
    }
    let bad = alice
        .call_json(Method::PUT, &lock, &json!({"lock_reason": "rude"}))
        .await;
    let refused = carol.call(Method::PUT, &lock).await;
    let events = alice.get(&format!("{issue}/events")).await.json();
    server.admin(Method::POST, "hooks/drain", None).await;
    let received = server.receiver.received().await;

    // And a wrong reason or a reader changes nothing, and each change is an event and a webhook
    assert_eq!(bad.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(bad.json()["errors"][0]["field"], "lock_reason");
    assert_eq!(refused.status, StatusCode::FORBIDDEN);
    assert_eq!(alice.get(&issue).await.json()["locked"], false);
    let events = events.as_array().unwrap();
    let named: Vec<_> = events
        .iter()
        .map(|e| e["event"].as_str().unwrap())
        .collect();
    assert_eq!(named, actions);
    assert_eq!(events[2]["lock_reason"], "too heated");
    assert!(events[8]["lock_reason"].is_null());
    assert!(events[1].get("lock_reason").is_none());
    assert!(events[0]["node_id"].as_str().unwrap().starts_with("LOE_"));
    assert!(events[1]["node_id"].as_str().unwrap().starts_with("UNLOE_"));
    let delivered: Vec<_> = received
        .iter()
        .filter(|d| d.header("x-github-event") == "issues")
        .map(|d| d.json())
        .filter(|p| p["action"] != "opened")
        .collect();
    let delivered_actions: Vec<_> = delivered
        .iter()
        .map(|p| p["action"].as_str().unwrap())
        .collect();
    assert_eq!(delivered_actions, actions);
    assert_eq!(delivered[4]["issue"]["active_lock_reason"], "resolved");
    assert_eq!(delivered[4]["issue"]["locked"], true);
}

#[tokio::test]
async fn a_file_comment_answers_line_and_position_1_and_an_empty_hunk_in_rest() {
    // Given a pull request from feature to main
    let pull = json!({"kind": "pull_request", "repository": "acme/public", "user": "alice",
        "title": "feature", "head": "feature", "base": "main"});
    let server = whole_server(&[pull]).await;
    let alice = GoGithub::new(&server).with_token(ALICE_TOKEN);
    let files = alice.get(&format!("{REPO}/pulls/1/files")).await.json();
    let path = files[0]["filename"].as_str().unwrap().to_owned();

    // When the client writes a review with a comment on the whole file and reads it back
    let review = alice
        .post(
            &format!("{REPO}/pulls/1/reviews"),
            &json!({"event": "COMMENT", "comments": [
                {"path": path, "subject_type": "file", "body": "file"}]}),
        )
        .await;
    let listed = alice.get(&format!("{REPO}/pulls/1/comments")).await.json();
    let id = listed[0]["id"].as_i64().unwrap();
    let single = alice
        .get(&format!("{REPO}/pulls/comments/{id}"))
        .await
        .json();

    // Then REST answers the fields live GitHub answers for a file comment
    assert_eq!(review.status, StatusCode::OK, "{}", review.json());
    for comment in [&listed[0], &single] {
        assert_eq!(comment["subject_type"], "file");
        for field in ["line", "original_line", "position", "original_position"] {
            assert_eq!(comment[field], 1, "{field}");
        }
        assert_eq!(comment["diff_hunk"], "");
        assert_eq!(comment["side"], "RIGHT");
        assert!(comment["start_line"].is_null());
        assert!(comment["start_side"].is_null());
    }
}

#[tokio::test]
async fn team_and_collaborator_reads_take_the_scopes_github_accepts() {
    // Given a team of acme, and alice's classic tokens with no scope and with each single scope
    let team = json!({"kind": "team", "org": "acme", "name": "Child",
        "members": [{"login": "bob"}],
        "repositories": [{"repository": "acme/public", "permission": "read"}]});
    let mut builder = TestServer::builder().frame(hubdouble_api::register_all);
    for mut entry in fixture::seed().into_iter().chain([team]) {
        if entry["kind"] == "user" && entry["login"] == ALICE {
            entry["tokens"] = json!([
                {"token": ALICE_TOKEN},
                {"token": NO_SCOPE_PAT, "scopes": []},
                {"token": "ghp_0000000000000000000000000000000000c2", "scopes": ["user"]},
                {"token": "ghp_0000000000000000000000000000000000c3", "scopes": ["repo"]},
                {"token": "ghp_0000000000000000000000000000000000c4", "scopes": ["read:user"]},
            ]);
        }
        builder = builder.entry(entry);
    }
    let server = builder.start().await;
    let client = |token: &str| GoGithub::new(&server).with_token(token);
    let reads = [
        (
            "orgs/acme/teams",
            "admin:org, read:org, repo, user, write:org",
        ),
        (
            "orgs/acme/teams/child",
            "admin:org, read:org, repo, user, write:org",
        ),
        (
            "orgs/acme/teams/child/repos/acme/public",
            "admin:org, read:org, repo, user, write:org",
        ),
        (
            "orgs/acme/teams/child/memberships/bob",
            "admin:org, read:org, repo, write:org",
        ),
        ("orgs/acme/members/bob", "read:org, repo, user"),
    ];

    for (path, accepted) in reads {
        // When the token without scopes and each token with one accepted scope read it
        let bare = client(NO_SCOPE_PAT).get(path).await;

        // Then only the scopes in GitHub's X-Accepted-OAuth-Scopes header let it through
        assert_eq!(bare.status, StatusCode::FORBIDDEN, "{path}");
        assert_eq!(bare.header("x-accepted-oauth-scopes"), accepted, "{path}");
        for (token, scope) in [
            ("ghp_0000000000000000000000000000000000c2", "user"),
            ("ghp_0000000000000000000000000000000000c3", "repo"),
            ("ghp_0000000000000000000000000000000000c4", "read:user"),
        ] {
            let answer = client(token).get(path).await;
            let accepts = accepted.split(", ").any(|s| s == scope);
            assert_eq!(answer.status.is_success(), accepts, "{path} {scope}");
        }
    }
    let permission = client(NO_SCOPE_PAT)
        .get("repos/acme/public/collaborators/bob/permission")
        .await;
    assert_eq!(permission.status, StatusCode::OK);
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

const SPOON_PATCH: &str = include_str!("live/spoon-knife-compare.patch");
const SPOON_DIFF: &str = include_str!("live/spoon-knife-compare.diff");
const RENAME_PATCH: &str = include_str!("live/gitignore-rename.patch");
const BINARY_DIFF: &str = include_str!("live/explore-binary.diff");
const SPOON: &str = "repos/acme/spoon";
const FIRST: &str = "bb4cc8d3b2e14b3af5df699876dd4ff3acd00b7f";
const SECOND: &str = "d0dd1f61b33d64e29d8bc1372a94ef6a2fee76a9";

fn added(diff: &str, path: &str) -> String {
    let start = diff.find(&format!("+++ b/{path}\n")).unwrap();
    let mut content = String::new();
    for line in diff[start..]
        .lines()
        .skip(2)
        .take_while(|line| line.starts_with('+'))
    {
        content.push_str(&line[1..]);
        content.push('\n');
    }
    content
}

fn abbreviated(text: &str) -> String {
    text.split_inclusive('\n')
        .map(|line| match line.strip_prefix("index ") {
            Some(rest) => {
                let end = rest.find([' ', '\n']).unwrap();
                let (a, b) = rest[..end].split_once("..").unwrap();
                format!("index {}..{}{}", &a[..7], &b[..7], &rest[end..])
            }
            None => line.to_owned(),
        })
        .collect()
}

fn octocat(message: &str, date: &str, files: &Value) -> Value {
    json!({"branch": "fork", "message": message, "files": files,
        "author": {"name": "The Octocat", "email": "octocat@nowhere.com", "date": date}})
}

async fn spoon_server() -> TestServer {
    let v1 = added(SPOON_PATCH, "README.md");
    let css = added(SPOON_PATCH, "styles.css");
    let v2 = added(SPOON_DIFF, "README.md");
    let ignore = json!({"community/Hardhat.gitignore": "node_modules\n"});
    let moved = json!({"community/Hardhat.gitignore": null,
        "community/JavaScript/Hardhat.gitignore": "node_modules\n"});
    let mut commits = vec![
        octocat(
            "Create styles.css and updated README\n",
            "2014-02-04T22:38:36Z",
            &json!({"README.md": v1, "styles.css": css}),
        ),
        octocat(
            "Pointing to the guide for forking\n",
            "2014-02-12T23:20:44Z",
            &json!({"README.md": v2}),
        ),
    ];
    for files in [
        ignore,
        moved,
        json!({"topics/git/git.png": "\u{0}a"}),
        json!({"topics/git/git.png": "\u{0}b"}),
    ] {
        commits.push(json!({"branch": "moves", "message": "Move", "files": files}));
    }
    let repo = json!({"kind": "repository", "owner": "acme", "name": "spoon",
        "visibility": "public", "files": {"index.html": "<html></html>\n"}, "commits": commits});
    let pull = json!({"kind": "pull_request", "repository": "acme/spoon", "user": "alice",
        "title": "fork", "head": "fork", "base": "main"});
    whole_server(&[repo, pull]).await
}

async fn text(client: &GoGithub, path: &str, accept: &'static str) -> (String, String, String) {
    let answer = client
        .call_json_headers(
            http::Method::GET,
            path,
            Vec::new(),
            headers(&[("accept", accept)]),
        )
        .await;
    assert_eq!(answer.status, StatusCode::OK, "{path} {accept}");
    (
        String::from_utf8(answer.body.to_vec()).unwrap(),
        answer.header("content-type").to_owned(),
        answer.header("x-github-media-type").to_owned(),
    )
}

async fn shas(client: &GoGithub, branch: &str) -> Vec<String> {
    let answer = client.get(&format!("{SPOON}/commits?sha={branch}")).await;
    let mut shas: Vec<String> = answer
        .json()
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["sha"].as_str().unwrap().to_owned())
        .collect();
    shas.reverse();
    shas
}

#[tokio::test]
async fn diff_and_patch_media_answer_the_bytes_github_writes() {
    // Given the two Spoon-Knife commits, seeded with their files, authors and dates in UTC
    let server = spoon_server().await;
    let alice = GoGithub::new(&server).with_token(ALICE_TOKEN);
    let fork = shas(&alice, "fork").await;
    let (first, second) = (&fork[1], &fork[2]);
    let live = abbreviated(SPOON_PATCH)
        .replace(FIRST, first)
        .replace(SECOND, second)
        .replace("4 Feb 2014 14:38:36 -0800", "4 Feb 2014 22:38:36 +0000")
        .replace("12 Feb 2014 15:20:44 -0800", "12 Feb 2014 23:20:44 +0000");
    let first_mail = &live[..live.find(&format!("\nFrom {second}")).unwrap()];

    // When the client asks a commit, the compare and the pull request for diff and patch text
    let commit = format!("{SPOON}/commits/{first}");
    let compare = format!("{SPOON}/compare/main...fork");
    let commit_diff = text(&alice, &commit, "application/vnd.github.diff").await;
    let commit_patch = text(&alice, &commit, "application/vnd.github.patch").await;
    let compare_diff = text(&alice, &compare, "application/vnd.github.v3.diff").await;
    let compare_patch = text(&alice, &compare, "application/vnd.github.patch").await;
    let pull_diff = text(
        &alice,
        &format!("{SPOON}/pulls/1"),
        "application/vnd.github.diff",
    )
    .await;
    let pull_patch = text(
        &alice,
        &format!("{SPOON}/pulls/1"),
        "application/vnd.github.v3.patch",
    )
    .await;

    // Then each body is live GitHub's, with the seed's SHAs, UTC dates and 7-character IDs
    let diff_type = "application/vnd.github.diff; charset=utf-8".to_owned();
    let patch_type = "application/vnd.github.patch; charset=utf-8".to_owned();
    assert_eq!(
        commit_diff,
        (
            first_mail[first_mail.find("diff --git").unwrap()..].to_owned(),
            diff_type.clone(),
            "github.v3; param=diff".to_owned()
        )
    );
    assert_eq!(commit_patch.0, first_mail.replace("[PATCH 1/2]", "[PATCH]"));
    assert_eq!(
        (commit_patch.1, commit_patch.2),
        (patch_type.clone(), "github.v3; param=patch".to_owned())
    );
    assert_eq!(compare_diff.0, abbreviated(SPOON_DIFF));
    assert_eq!(compare_patch.0, live);
    assert_eq!((pull_diff.0, pull_diff.1), (compare_diff.0, diff_type));
    assert_eq!((pull_patch.0, pull_patch.1), (live, patch_type));
}

async fn blob(client: &GoGithub, sha: &str) -> String {
    let answer = client.get(&format!("{SPOON}/commits/{sha}")).await;
    answer.json()["files"][0]["sha"]
        .as_str()
        .unwrap()
        .to_owned()
}

#[tokio::test]
async fn renames_and_binary_files_take_the_lines_github_writes() {
    // Given a rename of an unchanged file and a change of a binary file
    let server = spoon_server().await;
    let alice = GoGithub::new(&server).with_token(ALICE_TOKEN);
    let moves = shas(&alice, "moves").await;
    let (rename, binary) = (&moves[2], &moves[4]);
    let (old, new) = (blob(&alice, &moves[3]).await, blob(&alice, binary).await);

    // When the client asks the two commits for patch and diff text
    let commit = |sha: &str| format!("{SPOON}/commits/{sha}");
    let rename_patch = text(&alice, &commit(rename), "application/vnd.github.patch").await;
    let binary_diff = text(&alice, &commit(binary), "application/vnd.github.diff").await;
    let binary_patch = text(&alice, &commit(binary), "application/vnd.github.patch").await;

    // Then the rename and binary lines are live GitHub's, and the patch holds the full binary
    assert!(rename_patch.0.ends_with(RENAME_PATCH), "{}", rename_patch.0);
    assert_eq!(
        binary_diff.0,
        BINARY_DIFF.replace(
            "e88d9a3c6525..d141110537fa",
            &format!("{}..{}", &old[..7], &new[..7])
        )
    );
    let patch = binary_patch.0;
    assert!(
        patch.contains("\n topics/git/git.png | Bin 2 -> 2 bytes\n 1 file changed, 0 insertions(+), 0 deletions(-)\n\n"),
        "{patch}"
    );
    assert!(
        patch.contains(&format!(
            "\nindex {old}..{new} 100644\nGIT binary patch\nliteral 2\n"
        )),
        "{patch}"
    );
}
