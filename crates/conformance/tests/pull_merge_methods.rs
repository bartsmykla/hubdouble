use std::sync::Arc;

use http::{Method, StatusCode};
use hubdouble_git::{Commit, EntryMode, FileChange, Object, ObjectData, ObjectId, Signature};
use hubdouble_model::Timestamp;
use hubdouble_testkit::fixture::{self, ALICE_ID, ALICE_TOKEN, PUBLIC_REPO_ID};
use hubdouble_testkit::{GoGithub, Graphql, TestServer};
use serde_json::{Value, json};

const PULL: &str = "repos/acme/public/pulls/1";
const REPO: &str = "repos/acme/public";

async fn server(change: impl Fn(&mut Value), extra: Vec<Value>) -> TestServer {
    let mut entries = fixture::seed();
    for entry in &mut entries {
        if entry["kind"] == "repository" && entry["name"] == fixture::PUBLIC_REPO {
            change(entry);
        }
    }
    entries.extend(extra);
    entries.push(json!({"kind": "mergeability", "repository": "acme/public", "reads": 0}));
    entries.push(
        json!({"kind": "pull_request", "repository": "acme/public", "user": "alice",
        "title": "Ship the feature", "body": "Why it ships", "head": "feature", "base": "main"}),
    );
    let mut builder = TestServer::builder().frame(hubdouble_api::register_all);
    for entry in entries {
        builder = builder.entry(entry);
    }
    builder.start().await
}

fn bob() -> Signature {
    Signature {
        name: "Bob Builder".into(),
        email: "bob@builder.example".into(),
        time: Timestamp::SEED,
        offset_minutes: 0,
    }
}

fn push(server: &TestServer, branch: &str, path: &str, data: &'static str, message: &str) {
    let services = server.server.services();
    let sig = bob();
    services
        .store
        .update(|tx| {
            let parent =
                services
                    .git
                    .resolve_ref(tx, PUBLIC_REPO_ID, &format!("refs/heads/{branch}"))?;
            Ok(services.git.commit(
                tx,
                PUBLIC_REPO_ID,
                ALICE_ID,
                branch,
                Some(parent),
                &[FileChange::Write {
                    path: path.into(),
                    data: Arc::from(data.as_bytes()),
                    mode: EntryMode::File,
                }],
                message,
                &sig,
                &sig,
                false,
            )?)
        })
        .unwrap_or_else(|e| panic!("push {branch}: {e:?}"));
}

fn tree_of(server: &TestServer, tx: &hubdouble_store::Tx, commit: ObjectId) -> ObjectId {
    match server
        .server
        .services()
        .git
        .object(tx, PUBLIC_REPO_ID, commit)
    {
        Ok(Object {
            data: ObjectData::Commit(c),
            ..
        }) => c.tree,
        other => panic!("commit {commit}: {other:?}"),
    }
}

fn merge_main_into_feature(server: &TestServer, keep_feature_tree: bool) {
    let services = server.server.services();
    services
        .store
        .update(|tx| {
            let git = &services.git;
            let feature = git.resolve_ref(tx, PUBLIC_REPO_ID, "refs/heads/feature")?;
            let main = git.resolve_ref(tx, PUBLIC_REPO_ID, "refs/heads/main")?;
            let tree = if keep_feature_tree {
                tree_of(server, tx, feature)
            } else {
                let merged = git
                    .test_merge(tx, PUBLIC_REPO_ID, ALICE_ID, feature, main)?
                    .unwrap_or_else(|| panic!("main merges into feature"));
                tree_of(server, tx, merged)
            };
            let merge = git.write_commit(
                tx,
                PUBLIC_REPO_ID,
                &Commit {
                    tree,
                    parents: vec![feature, main],
                    author: bob(),
                    committer: bob(),
                    message: "Merge main into feature".into(),
                    gpg_signature: None,
                },
            )?;
            git.update_ref(
                tx,
                PUBLIC_REPO_ID,
                ALICE_ID,
                "refs/heads/feature",
                Some(feature),
                Some(merge),
                false,
            )?;
            Ok(())
        })
        .unwrap_or_else(|e| panic!("merge main into feature: {e:?}"));
}

fn push_empty(server: &TestServer, message: &str) {
    let services = server.server.services();
    let sig = bob();
    services
        .store
        .update(|tx| {
            let parent = services
                .git
                .resolve_ref(tx, PUBLIC_REPO_ID, "refs/heads/feature")?;
            Ok(services.git.commit(
                tx,
                PUBLIC_REPO_ID,
                ALICE_ID,
                "feature",
                Some(parent),
                &[],
                message,
                &sig,
                &sig,
                false,
            )?)
        })
        .unwrap_or_else(|e| panic!("push an empty commit: {e:?}"));
}

async fn json_of(client: &GoGithub, path: &str) -> Value {
    let answer = client.get(path).await;
    assert_eq!(answer.status, StatusCode::OK, "{path}: {}", answer.json());
    answer.json()
}

async fn merge(client: &GoGithub, body: &Value) -> (StatusCode, Value) {
    let answer = client
        .call_json(Method::PUT, &format!("{PULL}/merge"), body)
        .await;
    (answer.status, answer.json())
}

async fn diverged() -> (TestServer, GoGithub) {
    let server = server(|_| {}, Vec::new()).await;
    push(
        &server,
        "feature",
        "src/more.txt",
        "more\n",
        "Add more\n\nIt needs more.",
    );
    push(&server, "main", "docs/main.txt", "main\n", "Move main on");
    let alice = GoGithub::enterprise(&server).with_token(ALICE_TOKEN);
    (server, alice)
}

#[tokio::test]
async fn a_squash_merge_writes_one_web_flow_commit_with_the_default_message() {
    // Given a pull request of two commits by other authors, and a main that moved on
    let (server, alice) = diverged().await;
    let base = json_of(&alice, &format!("{REPO}/branches/main")).await["commit"]["sha"].clone();
    let computed = server
        .admin(Method::POST, "repos/acme/public/pulls/1/mergeability", None)
        .await;
    assert!(computed.status.is_success(), "{}", computed.json());
    let test_merge = json_of(&alice, PULL).await["merge_commit_sha"].clone();
    let tested_tree = json_of(
        &alice,
        &format!("{REPO}/git/commits/{}", test_merge.as_str().unwrap_or("")),
    )
    .await["tree"]["sha"]
        .clone();

    // When alice squashes it with the repository's default title and message
    let (status, merged) = merge(&alice, &json!({"merge_method": "squash"})).await;

    // Then main gains one commit on the old tip with the test merge tree, by the pull request author,
    // committed and signed by GitHub, titled and listed as live helix-editor/helix pull requests
    // 15803 and 16090 and neovim/neovim 42939 show
    assert_eq!(status, StatusCode::OK, "{merged}");
    let sha = merged["sha"].as_str().unwrap_or_default().to_owned();
    let commit = json_of(&alice, &format!("{REPO}/commits/{sha}")).await;
    assert_eq!(commit["parents"].as_array().map(Vec::len), Some(1));
    assert_eq!(commit["parents"][0]["sha"], base);
    assert_eq!(
        (
            &commit["commit"]["author"]["name"],
            &commit["commit"]["author"]["email"]
        ),
        (&json!("Alice"), &json!("alice@example.com"))
    );
    assert_eq!(
        (
            &commit["commit"]["committer"]["name"],
            &commit["commit"]["committer"]["email"]
        ),
        (&json!("GitHub"), &json!("noreply@github.com"))
    );
    assert_eq!(commit["commit"]["verification"]["verified"], true);
    assert_eq!(
        commit["commit"]["message"],
        "Ship the feature (#1)\n\n* add the feature\n\n* Add more\n\nIt needs more.\n\n\
         Co-authored-by: hubdouble <seed@hubdouble.local>\n\
         Co-authored-by: Bob Builder <bob@builder.example>"
    );
    let tree = json_of(&alice, &format!("{REPO}/git/commits/{sha}")).await["tree"]["sha"].clone();
    assert_eq!(tree, tested_tree);
    assert_eq!(
        json_of(&alice, &format!("{REPO}/branches/main")).await["commit"]["sha"],
        json!(sha)
    );
    let pull = json_of(&alice, PULL).await;
    assert_eq!(
        (&pull["state"], &pull["merged"], &pull["merge_commit_sha"]),
        (&json!("closed"), &json!(true), &json!(sha))
    );
    assert_eq!(pull["merged_by"]["login"], "alice");
    let events = json_of(&alice, "repos/acme/public/issues/1/events").await;
    let kinds: Vec<(&str, &Value)> = events
        .as_array()
        .into_iter()
        .flatten()
        .map(|e| (e["event"].as_str().unwrap_or_default(), &e["commit_id"]))
        .collect();
    assert!(kinds.contains(&("merged", &json!(sha))), "{events}");
    assert!(kinds.iter().any(|(kind, _)| *kind == "closed"), "{events}");
    let graph = Graphql::new(alice.clone())
        .query(
            "query($id: ID!) {node(id: $id) {... on PullRequest {state mergeCommit {oid}}}}",
            &json!({"id": pull["node_id"]}),
        )
        .await
        .json();
    assert_eq!(
        graph["data"]["node"],
        json!({"state": "MERGED", "mergeCommit": {"oid": sha}})
    );
    assert_eq!(
        server.admin(Method::POST, "hooks/drain", None).await.status,
        StatusCode::NO_CONTENT
    );
    let closed: Vec<Value> = server
        .receiver
        .received()
        .await
        .iter()
        .filter(|d| d.header("x-github-event") == "pull_request")
        .map(hubdouble_testkit::Delivery::json)
        .filter(|p| p["action"] == "closed")
        .collect();
    assert_eq!(closed.len(), 1);
    assert_eq!(
        (
            &closed[0]["pull_request"]["merged"],
            &closed[0]["pull_request"]["merge_commit_sha"]
        ),
        (&json!(true), &json!(sha))
    );
}

#[tokio::test]
async fn a_single_commit_squash_takes_the_commit_title_and_body() {
    // Given a pull request of one commit and a repository with the default squash settings
    let server = server(|_| {}, Vec::new()).await;
    let alice = GoGithub::enterprise(&server).with_token(ALICE_TOKEN);

    // When alice squashes it
    let (status, merged) = merge(&alice, &json!({"merge_method": "squash"})).await;

    // Then the title is the commit's with the number, as the merge docs say for one commit
    assert_eq!(status, StatusCode::OK, "{merged}");
    let commit = json_of(
        &alice,
        &format!("{REPO}/commits/{}", merged["sha"].as_str().unwrap_or("")),
    )
    .await;
    assert_eq!(
        commit["commit"]["message"],
        "add the feature (#1)\n\nCo-authored-by: hubdouble <seed@hubdouble.local>"
    );
}

#[tokio::test]
async fn a_rebase_merge_replays_each_commit_with_the_merger_as_committer() {
    // Given a pull request of two commits, and a main that moved on
    let (_server, alice) = diverged().await;
    let base = json_of(&alice, &format!("{REPO}/branches/main")).await["commit"]["sha"].clone();

    // When alice rebases it
    let (status, merged) = merge(&alice, &json!({"merge_method": "rebase"})).await;

    // Then main gains a copy of each commit with its author and message, committed by alice and
    // unsigned, as live helix-editor/helix pull request 15856 and junegunn/fzf pull request 4641 show
    assert_eq!(status, StatusCode::OK, "{merged}");
    let sha = merged["sha"].as_str().unwrap_or_default().to_owned();
    let last = json_of(&alice, &format!("{REPO}/commits/{sha}")).await;
    let first_sha = last["parents"][0]["sha"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    let first = json_of(&alice, &format!("{REPO}/commits/{first_sha}")).await;
    assert_eq!(first["parents"][0]["sha"], base);
    for (commit, message, author) in [
        (&first, "add the feature", "hubdouble"),
        (&last, "Add more\n\nIt needs more.", "Bob Builder"),
    ] {
        assert_eq!(commit["parents"].as_array().map(Vec::len), Some(1));
        assert_eq!(commit["commit"]["message"], message);
        assert_eq!(commit["commit"]["author"]["name"], author);
        assert_eq!(
            (
                &commit["commit"]["committer"]["name"],
                &commit["commit"]["committer"]["email"]
            ),
            (&json!("Alice"), &json!("alice@example.com"))
        );
        assert_eq!(commit["commit"]["verification"]["verified"], false);
    }
    let pull = json_of(&alice, PULL).await;
    assert_eq!(
        (&pull["merged"], &pull["merge_commit_sha"]),
        (&json!(true), &json!(sha))
    );
    let file = alice
        .get(&format!("{REPO}/contents/src/more.txt?ref=main"))
        .await;
    assert_eq!(file.status, StatusCode::OK);
}

#[tokio::test]
async fn a_rebase_that_does_not_apply_is_refused_and_moves_nothing() {
    // Given a main that changed the file the pull request changes
    let server = server(|_| {}, Vec::new()).await;
    push(
        &server,
        "main",
        "src/feature.txt",
        "other\n",
        "Change the same file",
    );
    let alice = GoGithub::enterprise(&server).with_token(ALICE_TOKEN);
    let base = json_of(&alice, &format!("{REPO}/branches/main")).await["commit"]["sha"].clone();

    // When alice rebases and squashes it
    let (rebase, _) = merge(&alice, &json!({"merge_method": "rebase"})).await;
    let (squash, _) = merge(&alice, &json!({"merge_method": "squash"})).await;

    // Then both are refused and main and the pull request stay as they were
    assert_eq!(
        (rebase, squash),
        (
            StatusCode::METHOD_NOT_ALLOWED,
            StatusCode::METHOD_NOT_ALLOWED
        )
    );
    assert_eq!(
        json_of(&alice, &format!("{REPO}/branches/main")).await["commit"]["sha"],
        base
    );
    assert_eq!(json_of(&alice, PULL).await["state"], "open");
}

#[tokio::test]
async fn a_rebase_leaves_out_merge_commits_and_commits_that_were_empty() {
    // Given a head that merged the moved main back in, then got an empty commit and one more
    let (server, alice) = diverged().await;
    let base = json_of(&alice, &format!("{REPO}/branches/main")).await["commit"]["sha"].clone();
    merge_main_into_feature(&server, false);
    push_empty(&server, "Nothing to see");
    push(&server, "feature", "src/last.txt", "last\n", "Add last");

    // When alice rebases it
    let (status, merged) = merge(&alice, &json!({"merge_method": "rebase"})).await;

    // Then main gains a copy of each commit that changed something, as the merge docs say
    assert_eq!(status, StatusCode::OK, "{merged}");
    let compared = json_of(
        &alice,
        &format!(
            "{REPO}/compare/{}...{}",
            base.as_str().unwrap_or_default(),
            merged["sha"].as_str().unwrap_or_default()
        ),
    )
    .await;
    let messages: Vec<&Value> = compared["commits"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|c| &c["commit"]["message"])
        .collect();
    assert_eq!(
        messages,
        [
            &json!("add the feature"),
            &json!("Add more\n\nIt needs more."),
            &json!("Add last"),
        ]
    );
}

#[tokio::test]
async fn a_rebase_that_gives_another_tree_than_the_merge_is_refused() {
    // Given a head whose merge of main kept only the head's files
    let (server, alice) = diverged().await;
    let base = json_of(&alice, &format!("{REPO}/branches/main")).await["commit"]["sha"].clone();
    merge_main_into_feature(&server, true);

    // When alice rebases it
    let (status, refused) = merge(&alice, &json!({"merge_method": "rebase"})).await;

    // Then the unsafe rebase is refused, as the merge docs say, and main stays as it was
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED, "{refused}");
    assert_eq!(
        json_of(&alice, &format!("{REPO}/branches/main")).await["commit"]["sha"],
        base
    );
    assert_eq!(json_of(&alice, PULL).await["state"], "open");
}

#[tokio::test]
async fn the_repository_settings_and_the_ruleset_choose_the_merge_methods() {
    // Given a repository without squash merges, and a ruleset that allows rebase merges only
    // and asks for a linear history
    let ruleset = json!({"kind": "ruleset", "repository": "acme/public", "name": "rules",
        "target": "branch", "enforcement": "active", "bypass_actors": [],
        "conditions": {"ref_name": {"include": ["~DEFAULT_BRANCH"], "exclude": []}},
        "rules": [{"type": "required_linear_history"}, {"type": "pull_request", "parameters": {
            "required_approving_review_count": 0, "dismiss_stale_reviews_on_push": false,
            "require_code_owner_review": false, "require_last_push_approval": false,
            "required_review_thread_resolution": false, "allowed_merge_methods": ["rebase"]}}]});
    let server = server(
        |repo| repo["allow_squash_merge"] = json!(false),
        vec![ruleset],
    )
    .await;
    let alice = GoGithub::enterprise(&server).with_token(ALICE_TOKEN);

    // When alice tries each method in turn
    let (squash, _) = merge(&alice, &json!({"merge_method": "squash"})).await;
    let (plain, _) = merge(&alice, &json!({"merge_method": "merge"})).await;
    let (rebase, merged) = merge(&alice, &json!({"merge_method": "rebase"})).await;

    // Then the setting refuses the squash, the ruleset refuses the merge commit, and the rebase merges
    assert_eq!(squash, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(plain, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(rebase, StatusCode::OK, "{merged}");
    assert_eq!(json_of(&alice, PULL).await["merged"], true);
}

async fn merge_as(alice: &GoGithub, method: &str, email: &str) -> Value {
    let id = json_of(alice, PULL).await["node_id"].clone();
    Graphql::new(alice.clone())
        .query(
            "mutation($id: ID!, $method: PullRequestMergeMethod!, $email: String!) {
             mergePullRequest(input: {pullRequestId: $id, mergeMethod: $method,
             authorEmail: $email}) {pullRequest {mergeCommit {oid}}}}",
            &json!({"id": id, "method": method, "email": email}),
        )
        .await
        .json()
}

#[tokio::test]
async fn graphql_merge_pull_request_refuses_an_author_email_of_another_account() {
    // Given a pull request by alice
    let (_server, alice) = diverged().await;

    // When alice merges it with an address that is not hers, and with an empty one
    let mut answers = Vec::new();
    for email in ["bob@builder.example", ""] {
        answers.push(merge_as(&alice, "MERGE", email).await);
    }

    // Then both are refused as cli/cli issue 14711 shows, and the pull request stays open
    for answer in &answers {
        assert_eq!(answer["data"]["mergePullRequest"], Value::Null, "{answer}");
        assert_eq!(answer["errors"][0]["type"], "UNPROCESSABLE", "{answer}");
        assert_eq!(answer["errors"][0]["path"], json!(["mergePullRequest"]));
    }
    assert_eq!(json_of(&alice, PULL).await["state"], "open");
}

#[tokio::test]
async fn graphql_merge_pull_request_writes_the_author_email_into_the_merge() {
    for (method, role) in [
        ("MERGE", "author"),
        ("SQUASH", "author"),
        ("REBASE", "committer"),
    ] {
        // Given a pull request by alice, who has her own address and a noreply address
        let (_server, alice) = diverged().await;
        let noreply = format!("{ALICE_ID}+alice@users.noreply.github.com");

        // When alice merges it with her noreply address
        let answer = merge_as(&alice, method, &noreply).await;

        // Then the commit that the merge writes for her carries that address
        let sha = answer["data"]["mergePullRequest"]["pullRequest"]["mergeCommit"]["oid"]
            .as_str()
            .unwrap_or_else(|| panic!("{method}: {answer}"))
            .to_owned();
        let commit = json_of(&alice, &format!("{REPO}/commits/{sha}")).await;
        assert_eq!(commit["commit"][role]["email"], json!(noreply), "{method}");
    }
}

#[tokio::test]
async fn graphql_merge_pull_request_squashes_with_the_given_headline() {
    // Given a pull request of two commits
    let (_server, alice) = diverged().await;
    let id = json_of(&alice, PULL).await["node_id"].clone();

    // When alice merges it through GraphQL with the SQUASH method and her own headline and body
    let answer = Graphql::new(alice.clone())
        .query(
            "mutation($id: ID!) {mergePullRequest(input: {pullRequestId: $id, mergeMethod: SQUASH,
             commitHeadline: \"Squashed\", commitBody: \"Done\"}) {pullRequest {state mergeCommit {oid}}}}",
            &json!({"id": id}),
        )
        .await
        .json();

    // Then the pull request is merged by one commit with that message and no added trailers
    let node = &answer["data"]["mergePullRequest"]["pullRequest"];
    assert_eq!(node["state"], "MERGED", "{answer}");
    let sha = node["mergeCommit"]["oid"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    let commit = json_of(&alice, &format!("{REPO}/commits/{sha}")).await;
    assert_eq!(commit["parents"].as_array().map(Vec::len), Some(1));
    assert_eq!(commit["commit"]["message"], "Squashed\n\nDone");
}
