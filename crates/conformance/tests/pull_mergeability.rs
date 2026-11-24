use std::fmt::Write as _;
use std::sync::Arc;

use http::{Method, StatusCode};
use hubdouble_git::{Commit, EntryMode, FileChange, Object, ObjectData, ObjectId, Signature};
use hubdouble_testkit::fixture::{ALICE_ID, ALICE_TOKEN, PUBLIC_REPO_ID, WEBHOOK_SECRET};
use hubdouble_testkit::validate::webhook_payload;
use hubdouble_testkit::{GoGithub, Graphql, TestServer};
use serde_json::{Value, json};

const PULL: &str = "repos/acme/public/pulls/1";

async fn server(extra: &[Value]) -> TestServer {
    let mut builder = TestServer::builder()
        .fixture()
        .register(hubdouble_api_gitdata::register)
        .register(hubdouble_api_repos::register)
        .register(hubdouble_api_pulls::register)
        .register(hubdouble_api_issues::register);
    for entry in extra {
        builder = builder.entry(entry.clone());
    }
    builder.start().await
}

fn pull_request() -> Value {
    json!({"kind": "pull_request", "repository": "acme/public", "user": "alice",
        "title": "feature into main", "head": "feature", "base": "main"})
}

fn alice(server: &TestServer) -> GoGithub {
    GoGithub::enterprise(server).with_token(ALICE_TOKEN)
}

fn tip(server: &TestServer, branch: &str) -> ObjectId {
    let services = server.server.services();
    services
        .store
        .view(|tx| {
            Ok(services
                .git
                .resolve_ref(tx, PUBLIC_REPO_ID, &format!("refs/heads/{branch}"))?)
        })
        .unwrap_or_else(|e| panic!("read {branch}: {e:?}"))
}

fn push(server: &TestServer, branch: &str, path: &str, text: &str) -> ObjectId {
    let services = server.server.services();
    let parent = tip(server, branch);
    let sig = Signature::seed();
    services
        .store
        .update(|tx| {
            Ok(services.git.commit(
                tx,
                PUBLIC_REPO_ID,
                ALICE_ID,
                branch,
                Some(parent),
                &[FileChange::Write {
                    path: path.into(),
                    data: Arc::from(text.as_bytes()),
                    mode: EntryMode::File,
                }],
                &format!("change {path}"),
                &sig,
                &sig,
                false,
            )?)
        })
        .unwrap_or_else(|e| panic!("push {branch}: {e:?}"))
}

fn pending(pull: &Value) -> Value {
    json!([
        pull["mergeable"],
        pull["mergeable_state"],
        pull["rebaseable"]
    ])
}

async fn conflicted(client: &GoGithub) -> (bool, usize) {
    for reads in 1..=10 {
        let pull = client.get(PULL).await.json();
        assert_eq!(pull["state"], "open");
        if let Some(mergeable) = pull["mergeable"].as_bool() {
            return (!mergeable, reads);
        }
    }
    panic!("mergeable is still null after 10 reads");
}

async fn graphql_state(client: &GoGithub, node: &Value) -> Value {
    let answer = Graphql::new(client.clone())
        .query(
            "query($id: ID!) {node(id: $id) {... on PullRequest {mergeable mergeStateStatus baseRefOid}}}",
            &json!({"id": node}),
        )
        .await
        .json();
    assert_eq!(answer.get("errors"), None, "{answer}");
    answer["data"]["node"].clone()
}

fn merge_main_into_feature(server: &TestServer, keep_feature_tree: bool) {
    let services = server.server.services();
    let git = &services.git;
    let tree_of = |tx: &hubdouble_store::Tx, id: ObjectId| match git.object(tx, PUBLIC_REPO_ID, id)
    {
        Ok(Object {
            data: ObjectData::Commit(c),
            ..
        }) => c.tree,
        other => panic!("commit {id}: {other:?}"),
    };
    let (feature, main) = (tip(server, "feature"), tip(server, "main"));
    services
        .store
        .update(|tx| {
            let tree = if keep_feature_tree {
                tree_of(tx, feature)
            } else {
                let merged = git
                    .test_merge(tx, PUBLIC_REPO_ID, ALICE_ID, feature, main)?
                    .unwrap_or_else(|| panic!("main merges into feature"));
                tree_of(tx, merged)
            };
            let merge = git.write_commit(
                tx,
                PUBLIC_REPO_ID,
                &Commit {
                    tree,
                    parents: vec![feature, main],
                    author: Signature::seed(),
                    committer: Signature::seed(),
                    message: "Merge main into feature".into(),
                    gpg_signature: None,
                },
            )?;
            Ok(git.update_ref(
                tx,
                PUBLIC_REPO_ID,
                ALICE_ID,
                "refs/heads/feature",
                Some(feature),
                Some(merge),
                false,
            )?)
        })
        .unwrap_or_else(|e| panic!("merge main into feature: {e:?}"));
}

async fn graphql_rebase(client: &GoGithub, node: &Value, fields: &str) -> Value {
    let answer = Graphql::new(client.clone())
        .query(
            &format!("query($id: ID!) {{node(id: $id) {{... on PullRequest {{{fields}}}}}}}"),
            &json!({"id": node}),
        )
        .await
        .json();
    assert_eq!(answer.get("errors"), None, "{answer}");
    answer["data"]["node"].clone()
}

#[tokio::test]
async fn a_head_whose_merge_commit_makes_the_rebase_unsafe_is_not_rebaseable() {
    for (keep_feature_tree, rebaseable) in [(true, false), (false, true)] {
        // Given an open pull request whose head then merges the moved main back in
        let server = server(&[pull_request()]).await;
        let client = alice(&server);
        let old = client.settled_pull(PULL).await;
        push(&server, "main", "docs/main.txt", "main\n");
        merge_main_into_feature(&server, keep_feature_tree);

        // When the client reads only canBeRebased as often as a computation takes reads
        let waiting = client.get(PULL).await.json();
        for _ in 0..2 {
            graphql_rebase(&client, &old["node_id"], "canBeRebased").await;
        }
        let pull = client.get(PULL).await.json();
        let node = graphql_rebase(&client, &old["node_id"], "mergeable canBeRebased").await;

        // Then rebaseable is null while mergeable is, canBeRebased reads count as mergeable
        // reads do, and a merge commit that keeps only the head's files makes the rebase
        // unsafe, as the merge docs say
        assert_eq!(pending(&waiting), json!([null, "unknown", null]));
        assert_eq!(pull["mergeable"], true, "{pull}");
        assert_eq!(pull["rebaseable"], rebaseable, "{pull}");
        assert_eq!(
            node,
            json!({"mergeable": "MERGEABLE", "canBeRebased": rebaseable})
        );
    }
}

#[tokio::test]
async fn the_client_merge_check_retries_a_null_mergeable_to_its_answer() {
    // Given an open pull request whose base and head then change README.md differently
    let server = server(&[pull_request()]).await;
    let client = alice(&server);
    let old = client.settled_pull(PULL).await;
    let base = push(&server, "main", "README.md", "# from main\n");
    let head = push(&server, "feature", "README.md", "# from the feature\n");
    // When the first two reads come right after the push
    let first = client.get(PULL).await.json();
    let second = client.get(PULL).await.json();
    // Then both answer null, unknown and null, with the new head on the live base and the last test merge, as live rust-lang/rust 166365 did
    for read in [&first, &second] {
        assert_eq!(pending(read), json!([null, "unknown", null]));
        assert_eq!(read["head"]["sha"], json!(head));
        assert_eq!(read["base"]["sha"], json!(base));
        assert_eq!(read["merge_commit_sha"], old["merge_commit_sha"]);
    }
    // When the client's merge check reads until mergeable is not null
    let (conflict, reads) = conflicted(&client).await;
    // Then its next read answers that the pull request no longer merges
    assert_eq!((conflict, reads), (true, 1));
    let pull = client.get(PULL).await.json();
    assert_eq!(pending(&pull), json!([false, "dirty", false]));
    assert_eq!(pull["merge_commit_sha"], Value::Null);
    // When the head moves again to a clean change and the client checks from the first read
    push(&server, "feature", "README.md", "# from main\n");
    let (conflict, reads) = conflicted(&client).await;
    // Then it retries past two null answers and learns that the pull request merges
    assert_eq!((conflict, reads), (false, 3));
}

#[tokio::test]
async fn a_head_push_gives_null_mergeability_to_its_webhook_and_graphql_while_pending() {
    // Given an open pull request with a computed test merge
    let server = server(&[pull_request()]).await;
    let client = alice(&server);
    let old = client.settled_pull(PULL).await;
    // When its head moves
    let head = push(&server, "feature", "src/next.txt", "next\n");
    // Then GraphQL answers UNKNOWN for mergeable and mergeStateStatus on the live base
    let node = graphql_state(&client, &old["node_id"]).await;
    assert_eq!(
        node,
        json!({"mergeable": "UNKNOWN", "mergeStateStatus": "UNKNOWN",
            "baseRefOid": tip(&server, "main")})
    );
    // Then the synchronize webhook carries null, unknown and null with the last test merge
    let drained = server.admin(Method::POST, "hooks/drain", None).await;
    assert_eq!(drained.status, StatusCode::NO_CONTENT);
    let all = server.receiver.received().await;
    let sync: Vec<_> = all
        .iter()
        .filter(|d| d.header("x-github-event") == "pull_request")
        .map(|d| {
            assert!(d.signed_with(WEBHOOK_SECRET));
            d.json()
        })
        .filter(|p| p["action"] == "synchronize")
        .collect();
    assert_eq!(sync.len(), 1);
    webhook_payload("pull_request", Some("synchronize"), &sync[0])
        .unwrap_or_else(|errors| panic!("{errors:?}"));
    let payload = &sync[0]["pull_request"];
    assert_eq!(pending(payload), json!([null, "unknown", null]));
    assert_eq!(payload["head"]["sha"], json!(head));
    assert_eq!(payload["merge_commit_sha"], old["merge_commit_sha"]);
    // When the computation ends
    let pull = client.settled_pull(PULL).await;
    // Then GraphQL answers from the new test merge
    assert_eq!(pull["mergeable"], true);
    let node = graphql_state(&client, &old["node_id"]).await;
    assert_eq!(node["mergeable"], "MERGEABLE");
    assert_eq!(node["mergeStateStatus"], "CLEAN");
}

#[tokio::test]
async fn the_reads_a_computation_takes_come_from_the_seed_and_the_admin_api() {
    // Given a repository whose seed computes mergeability before any read
    let reads = |n: u32| json!({"kind": "mergeability", "repository": "acme/public", "reads": n});
    let server = server(&[reads(0), pull_request()]).await;
    let client = alice(&server);
    // When the head moves
    push(&server, "feature", "src/next.txt", "next\n");
    // Then the first read answers the computed mergeability
    assert_eq!(conflicted(&client).await, (false, 1));
    // When the admin API sets four reads and the head moves again
    let mut body = reads(4);
    body.as_object_mut().map(|o| o.remove("kind"));
    let set = server
        .admin(Method::POST, "mergeability", Some(&body))
        .await;
    assert_eq!(set.status, StatusCode::CREATED, "{}", set.json());
    push(&server, "feature", "src/next.txt", "again\n");
    // Then four reads answer null before the fifth answers
    assert_eq!(conflicted(&client).await, (false, 5));
    // When the head moves once more and the admin settles the computation
    push(&server, "feature", "src/next.txt", "once more\n");
    let settled = server
        .admin(Method::POST, "repos/acme/public/pulls/1/mergeability", None)
        .await;
    // Then the admin answer and the first read both carry the computed mergeability
    assert_eq!(settled.status, StatusCode::OK, "{}", settled.json());
    assert_eq!(settled.json()["mergeable"], true);
    assert_eq!(conflicted(&client).await, (false, 1));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parallel_reads_from_many_clients_each_count_once() {
    // Given a pending computation that takes as many reads as there are clients
    const CLIENTS: u32 = 16;
    let reads = json!({"kind": "mergeability", "repository": "acme/public", "reads": CLIENTS});
    let server = server(&[reads, pull_request()]).await;
    let client = alice(&server);
    let node = client.settled_pull(PULL).await["node_id"].clone();
    push(&server, "feature", "src/next.txt", "next\n");
    let mut others = String::new();
    for i in 0..100 {
        write!(
            others,
            " t{i}: node(id: $id) {{... on PullRequest {{title body headRefOid}}}}"
        )
        .unwrap();
    }
    let query =
        format!("query($id: ID!) {{node(id: $id) {{... on PullRequest {{mergeable}}}}{others}}}");
    // When each client reads mergeable at the same time, in a query that goes on after the read
    let mut reading = tokio::task::JoinSet::new();
    for _ in 0..CLIENTS {
        let graphql = Graphql::new(alice(&server));
        let (query, node) = (query.clone(), node.clone());
        reading.spawn(async move { graphql.query(&query, &json!({"id": node})).await.json() });
    }
    let answers = reading.join_all().await;
    // Then every read answers UNKNOWN, and the read after them answers the computed value
    for answer in &answers {
        assert_eq!(answer.get("errors"), None, "{answer}");
        assert_eq!(answer["data"]["node"]["mergeable"], "UNKNOWN");
    }
    let pull = client.get(PULL).await.json();
    assert_eq!(pending(&pull), json!([true, "clean", true]));
}

#[tokio::test]
async fn a_read_of_an_older_head_does_not_count_for_the_computation_of_a_newer_push() {
    // Given a pending computation after a head push
    let server = server(&[pull_request()]).await;
    let client = alice(&server);
    let node = client.settled_pull(PULL).await["node_id"].clone();
    let head = push(&server, "feature", "src/next.txt", "next\n");
    // When one request reads mergeable of that head and then pushes the head again before its read job runs
    let answer = Graphql::new(client.clone())
        .query(
            "mutation($id: ID!, $commit: CreateCommitOnBranchInput!) {\
             read: convertPullRequestToDraft(input: {pullRequestId: $id}) {pullRequest {mergeable headRefOid}}\
             push: createCommitOnBranch(input: $commit) {commit {oid}}}",
            &json!({"id": node, "commit": {
                "branch": {"repositoryNameWithOwner": "acme/public", "branchName": "feature"},
                "expectedHeadOid": head,
                "message": {"headline": "push again"},
                "fileChanges": {"additions": [{"path": "src/next.txt", "contents": "YWdhaW4K"}]}
            }}),
        )
        .await
        .json();
    assert_eq!(answer.get("errors"), None, "{answer}");
    assert_eq!(
        answer["data"]["read"]["pullRequest"]["mergeable"],
        "UNKNOWN"
    );
    assert_eq!(
        answer["data"]["read"]["pullRequest"]["headRefOid"],
        json!(head)
    );
    // Then the newer push still takes its two reads, and the third read answers its computed mergeability
    assert_eq!(conflicted(&client).await, (false, 3));
    let pull = client.get(PULL).await.json();
    assert_eq!(pull["head"]["sha"], answer["data"]["push"]["commit"]["oid"]);
}

#[tokio::test]
async fn a_new_pull_request_answers_null_mergeability_until_it_is_computed() {
    // Given a branch ahead of main
    let server = server(&[]).await;
    let client = alice(&server);
    // When a pull request is opened from it
    let created = client
        .post(
            "repos/acme/public/pulls",
            &json!({"title": "feature", "head": "feature", "base": "main"}),
        )
        .await;
    // Then the answer has no mergeability and no test merge yet, as GitHub's opened payload has
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.json());
    let created = created.json();
    assert_eq!(pending(&created), json!([null, "unknown", null]));
    assert_eq!(created["merge_commit_sha"], Value::Null);
    let number = created["number"].as_i64().unwrap_or_default();
    let merge_ref = format!("repos/acme/public/git/ref/pull/{number}/merge");
    assert_eq!(client.get(&merge_ref).await.status, StatusCode::NOT_FOUND);
    // When reads go on until it is computed
    let pull = client
        .settled_pull(&format!("repos/acme/public/pulls/{number}"))
        .await;
    // Then it merges, and the merge ref names its test merge
    assert_eq!(pending(&pull), json!([true, "clean", true]));
    let merge = client.get(&merge_ref).await.json();
    assert_eq!(merge["object"]["sha"], pull["merge_commit_sha"]);
}
