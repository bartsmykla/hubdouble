use std::sync::Arc;

use http::{Method, StatusCode};
use hubdouble_git::{EntryMode, FileChange, ObjectId, Signature};
use hubdouble_testkit::fixture::{
    self, ALICE_ID, ALICE_TOKEN, BOB_TOKEN, CAROL_TOKEN, PUBLIC_REPO_ID,
};
use hubdouble_testkit::{GoGithub, Graphql, TestServer};
use serde_json::{Value, json};

const REPO: &str = "repos/acme/public";

const COMMON_KEYS: [&str; 9] = [
    "actor",
    "commit_id",
    "commit_url",
    "created_at",
    "event",
    "id",
    "node_id",
    "performed_via_github_app",
    "url",
];

const TEAM_KEYS: [&str; 15] = [
    "description",
    "html_url",
    "id",
    "members_url",
    "name",
    "node_id",
    "notification_setting",
    "organization_id",
    "parent",
    "permission",
    "privacy",
    "repositories_url",
    "slug",
    "type",
    "url",
];

async fn server() -> TestServer {
    server_with(&[]).await
}

async fn server_with(extra: &[Value]) -> TestServer {
    let mut entries = fixture::seed();
    for entry in &mut entries {
        if entry["kind"] == "repository" && entry["name"] == fixture::PUBLIC_REPO {
            entry["collaborators"] = json!([{"login": "bob", "permission": "write"}]);
        }
    }
    entries.push(json!({"kind": "team", "org": "acme", "name": "Reviewers",
        "privacy": "closed", "members": [{"login": "bob"}],
        "repositories": [{"repository": "acme/public", "permission": "write"}]}));
    entries.extend(extra.iter().cloned());
    let mut builder = TestServer::builder().frame(hubdouble_api::register_all);
    for entry in entries {
        builder = builder.entry(entry);
    }
    builder.start().await
}

fn alice(server: &TestServer) -> GoGithub {
    GoGithub::enterprise(server).with_token(ALICE_TOKEN)
}

async fn open(client: &GoGithub, draft: bool) -> Value {
    let opened = client
        .post(
            &format!("{REPO}/pulls"),
            &json!({"title": "feature", "head": "feature", "base": "main", "draft": draft}),
        )
        .await;
    assert_eq!(opened.status, StatusCode::CREATED, "{}", opened.json());
    opened.json()
}

async fn events(client: &GoGithub, number: &Value) -> Vec<Value> {
    events_in(client, REPO, number).await
}

async fn events_in(client: &GoGithub, repo: &str, number: &Value) -> Vec<Value> {
    let answer = client.get(&format!("{repo}/issues/{number}/events")).await;
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.json());
    answer.json().as_array().cloned().unwrap_or_default()
}

fn subscriptions(events: &[Value]) -> Vec<(&str, &str)> {
    events
        .iter()
        .filter(|e| e["event"] == "mentioned" || e["event"] == "subscribed")
        .map(|e| {
            (
                e["event"].as_str().unwrap_or_default(),
                e["actor"]["login"].as_str().unwrap_or_default(),
            )
        })
        .collect()
}

fn named<'a>(events: &'a [Value], name: &str) -> Vec<&'a Value> {
    events.iter().filter(|e| e["event"] == name).collect()
}

fn keys(value: &Value) -> Vec<String> {
    let mut keys: Vec<_> = value
        .as_object()
        .map_or_default(|o| o.keys().cloned().collect());
    keys.sort();
    keys
}

fn keys_with(extra: &[&str]) -> Vec<String> {
    let mut keys: Vec<String> = COMMON_KEYS
        .iter()
        .chain(extra)
        .map(|k| (*k).to_owned())
        .collect();
    keys.sort();
    keys
}

fn prefix(event: &Value) -> Option<&str> {
    event["node_id"].as_str()?.split_once('_').map(|(p, _)| p)
}

async fn reviewers(client: &GoGithub, number: &Value, method: Method) {
    let expected = if method == Method::POST {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    let answer = client
        .call_json(
            method,
            &format!("{REPO}/pulls/{number}/requested_reviewers"),
            &json!({"reviewers": ["bob"], "team_reviewers": ["reviewers"]}),
        )
        .await;
    assert_eq!(answer.status, expected, "{}", answer.json());
}

fn assert_review_request(events: &[Value], name: &str, node_prefix: &str) {
    let found = named(events, name);
    assert_eq!(found.len(), 2, "{name}: {events:?}");
    let (user, team) = (found[0], found[1]);
    assert_eq!(
        keys(user),
        keys_with(&["requested_reviewer", "review_requester"])
    );
    assert_eq!(
        keys(team),
        keys_with(&["requested_team", "review_requester"])
    );
    for event in [user, team] {
        assert_eq!(prefix(event), Some(node_prefix), "{name}");
        assert_eq!(event["review_requester"]["login"], "alice", "{name}");
        assert_eq!(event["actor"]["login"], "alice", "{name}");
        assert_eq!(event["commit_id"], Value::Null, "{name}");
        assert_eq!(event["commit_url"], Value::Null, "{name}");
    }
    assert_eq!(user["requested_reviewer"]["login"], "bob", "{name}");
    assert_eq!(keys(&team["requested_team"]), TEAM_KEYS.map(String::from));
    assert_eq!(team["requested_team"]["slug"], "reviewers", "{name}");
    assert_eq!(team["requested_team"]["type"], "organization", "{name}");
    assert_eq!(
        team["requested_team"]["organization_id"],
        json!(fixture::ORG_ID),
        "{name}"
    );
}

#[tokio::test]
async fn a_review_request_records_review_requested_for_a_user_and_a_team() {
    // Given an open pull request of alice
    let server = server().await;
    let alice = alice(&server);
    let pull = open(&alice, false).await;
    // When alice requests reviews from bob and the team reviewers
    reviewers(&alice, &pull["number"], Method::POST).await;
    // Then the issue events list one review_requested event for each, by alice
    assert_review_request(
        &events(&alice, &pull["number"]).await,
        "review_requested",
        "RRE",
    );
}

#[tokio::test]
async fn removing_a_review_request_records_review_request_removed_for_a_user_and_a_team() {
    // Given an open pull request of alice with reviews requested from bob and a team
    let server = server().await;
    let alice = alice(&server);
    let pull = open(&alice, false).await;
    reviewers(&alice, &pull["number"], Method::POST).await;
    // When alice removes both requests
    reviewers(&alice, &pull["number"], Method::DELETE).await;
    // Then the issue events list one review_request_removed event for each, after the requests
    let recorded = events(&alice, &pull["number"]).await;
    assert_review_request(&recorded, "review_request_removed", "RRRE");
    let names: Vec<&Value> = recorded.iter().map(|e| &e["event"]).collect();
    assert_eq!(
        names,
        [
            "review_requested",
            "review_requested",
            "review_request_removed",
            "review_request_removed"
        ]
    );
}

async fn set_draft(client: &GoGithub, pull: &Value, draft: bool) {
    let mutation = if draft {
        "convertPullRequestToDraft"
    } else {
        "markPullRequestReadyForReview"
    };
    let answer = Graphql::new(client.clone())
        .query(
            &format!(
                "mutation($id: ID!) {{ {mutation}(input: {{pullRequestId: $id}}) \
                 {{ pullRequest {{ isDraft }} }} }}"
            ),
            &json!({"id": pull["node_id"]}),
        )
        .await
        .json();
    assert_eq!(
        answer["data"][mutation]["pullRequest"]["isDraft"], draft,
        "{answer}"
    );
}

fn assert_plain(events: &[Value], name: &str, node_prefix: &str) {
    let found = named(events, name);
    assert_eq!(found.len(), 1, "{name}: {events:?}");
    let event = found[0];
    assert_eq!(keys(event), keys_with(&[]), "{name}");
    assert_eq!(prefix(event), Some(node_prefix), "{name}");
    assert_eq!(event["actor"]["login"], "alice", "{name}");
    assert_eq!(event["commit_id"], Value::Null, "{name}");
}

#[tokio::test]
async fn marking_a_draft_ready_records_ready_for_review() {
    // Given a draft pull request of alice
    let server = server().await;
    let alice = alice(&server);
    let pull = open(&alice, true).await;
    // When alice marks it ready for review, and again when it is ready
    set_draft(&alice, &pull, false).await;
    set_draft(&alice, &pull, false).await;
    // Then the issue events list one ready_for_review event, by alice
    let recorded = events(&alice, &pull["number"]).await;
    assert_plain(&recorded, "ready_for_review", "RFRE");
    let changed = alice.get(&format!("{REPO}/pulls/{}", pull["number"])).await;
    assert_eq!(
        named(&recorded, "ready_for_review")[0]["created_at"],
        changed.json()["updated_at"]
    );
}

#[tokio::test]
async fn converting_a_ready_pull_request_to_a_draft_records_convert_to_draft() {
    // Given a ready pull request of alice
    let server = server().await;
    let alice = alice(&server);
    let pull = open(&alice, false).await;
    // When alice converts it to a draft, and again when it is a draft
    set_draft(&alice, &pull, true).await;
    set_draft(&alice, &pull, true).await;
    // Then the issue events list one convert_to_draft event, by alice
    assert_plain(
        &events(&alice, &pull["number"]).await,
        "convert_to_draft",
        "CTDE",
    );
}

fn commit(server: &TestServer, branch: &str, parent: ObjectId, path: &str) -> ObjectId {
    commit_in(server, PUBLIC_REPO_ID, branch, parent, path)
}

fn commit_in(
    server: &TestServer,
    repo: i64,
    branch: &str,
    parent: ObjectId,
    path: &str,
) -> ObjectId {
    let services = server.server.services();
    let sig = Signature::seed();
    services
        .store
        .update(|tx| {
            Ok(services.git.commit(
                tx,
                repo,
                ALICE_ID,
                branch,
                Some(parent),
                &[FileChange::Write {
                    path: path.into(),
                    data: Arc::from(b"changed\n".as_slice()),
                    mode: EntryMode::File,
                }],
                &format!("change {path}"),
                &sig,
                &sig,
                false,
            )?)
        })
        .unwrap_or_else(|e| panic!("commit on {branch}: {e:?}"))
}

async fn tip(client: &GoGithub, branch: &str) -> ObjectId {
    tip_in(client, REPO, branch).await
}

async fn tip_in(client: &GoGithub, repo: &str, branch: &str) -> ObjectId {
    let answer = client.get(&format!("{repo}/git/ref/heads/{branch}")).await;
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.json());
    answer.json()["object"]["sha"]
        .as_str()
        .and_then(|sha| sha.parse().ok())
        .unwrap_or(ObjectId::NULL)
}

#[tokio::test]
async fn a_head_push_that_drops_the_old_head_records_head_ref_force_pushed_at_the_new_head() {
    // Given an open pull request of feature
    let server = server().await;
    let alice = alice(&server);
    let pull = open(&alice, false).await;
    // When feature moves forward, and then is forced to a commit on main that drops its head
    let forward = commit(&server, "feature", tip(&alice, "feature").await, "a.txt");
    let created = alice
        .post(
            &format!("{REPO}/git/refs"),
            &json!({"ref": "refs/heads/rewrite", "sha": tip(&alice, "main").await.to_string()}),
        )
        .await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.json());
    let rewrite = commit(&server, "rewrite", tip(&alice, "main").await, "b.txt");
    let forced = alice
        .call_json(
            Method::PATCH,
            &format!("{REPO}/git/refs/heads/feature"),
            &json!({"sha": rewrite.to_string(), "force": true}),
        )
        .await;
    assert_eq!(forced.status, StatusCode::OK, "{}", forced.json());
    // Then only the forced push records head_ref_force_pushed, with the new head as its commit
    let recorded = events(&alice, &pull["number"]).await;
    let found = named(&recorded, "head_ref_force_pushed");
    assert_eq!(found.len(), 1, "{recorded:?}");
    let event = found[0];
    assert_eq!(keys(event), keys_with(&[]));
    assert_eq!(prefix(event), Some("HRFPE"));
    assert_eq!(event["actor"]["login"], "alice");
    assert_eq!(event["commit_id"], json!(rewrite.to_string()));
    assert_ne!(event["commit_id"], json!(forward.to_string()));
    let url = event["commit_url"].as_str().unwrap_or_default();
    assert!(
        url.ends_with(&format!("/repos/acme/public/commits/{rewrite}")),
        "{url}"
    );
}

#[tokio::test]
async fn a_force_push_to_a_fork_head_names_the_fork_in_its_commit_url() {
    // Given an open pull request from bob's fork
    const FORK_ID: i64 = 3100;
    let fork = json!({"kind": "repository", "id": FORK_ID, "owner": "bob", "name": "public",
        "fork_of": "acme/public",
        "commits": [{"branch": "fork-work", "message": "fork change",
                     "files": {"fork.txt": "fork\n"}}]});
    let pull = json!({"kind": "pull_request", "repository": "acme/public", "user": "bob",
        "title": "fork work", "head": "bob:fork-work", "base": "main"});
    let server = server_with(&[fork, pull]).await;
    let alice = alice(&server);
    let bob = GoGithub::enterprise(&server).with_token(BOB_TOKEN);
    let listed = alice.get(&format!("{REPO}/pulls?head=bob:fork-work")).await;
    let number = listed.json()[0]["number"].clone();
    // When bob forces the fork head to a commit on the fork's main that drops it
    let fork_repo = "repos/bob/public";
    let fork_main = tip_in(&bob, fork_repo, "main").await;
    let created = bob
        .post(
            &format!("{fork_repo}/git/refs"),
            &json!({"ref": "refs/heads/rewrite", "sha": fork_main.to_string()}),
        )
        .await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.json());
    let rewrite = commit_in(&server, FORK_ID, "rewrite", fork_main, "b.txt");
    let forced = bob
        .call_json(
            Method::PATCH,
            &format!("{fork_repo}/git/refs/heads/fork-work"),
            &json!({"sha": rewrite.to_string(), "force": true}),
        )
        .await;
    assert_eq!(forced.status, StatusCode::OK, "{}", forced.json());
    // Then head_ref_force_pushed names the commit in the head repository, as live GitHub does
    // for rust-lang/rust pull 166526 (event 37297689504, 2026-11-25)
    let recorded = events(&alice, &number).await;
    let found = named(&recorded, "head_ref_force_pushed");
    assert_eq!(found.len(), 1, "{recorded:?}");
    assert_eq!(found[0]["commit_id"], json!(rewrite.to_string()));
    let url = found[0]["commit_url"].as_str().unwrap_or_default();
    assert!(
        url.ends_with(&format!("/repos/bob/public/commits/{rewrite}")),
        "{url}"
    );
}

#[tokio::test]
async fn each_text_that_mentions_a_user_records_a_pair_and_a_team_mention_only_subscribed() {
    // Given an issue whose body mentions carol, and code that names dave
    let server = server().await;
    let alice = alice(&server);
    let opened = alice
        .post(
            &format!("{REPO}/issues"),
            &json!({"title": "A fault", "body": "@carol please look, not `@dave`"}),
        )
        .await;
    assert_eq!(opened.status, StatusCode::CREATED, "{}", opened.json());
    let number = opened.json()["number"].clone();
    // When a comment mentions the reviewers team and carol again, a comment quotes carol and
    // mentions its author, and an edit names only carol, whom the body already names
    for body in [
        "cc @acme/reviewers and @carol",
        "> @carol said so\n\nI agree, @alice",
    ] {
        let commented = alice
            .post(
                &format!("{REPO}/issues/{number}/comments"),
                &json!({ "body": body }),
            )
            .await;
        assert_eq!(
            commented.status,
            StatusCode::CREATED,
            "{}",
            commented.json()
        );
    }
    let edited = alice
        .call_json(
            Method::PATCH,
            &format!("{REPO}/issues/{number}"),
            &json!({"body": "@carol, @carol please look"}),
        )
        .await;
    assert_eq!(edited.status, StatusCode::OK, "{}", edited.json());
    // Then each text gives its own pair for each user it names, as live GitHub did for
    // kubernetes/kubernetes 144288 and cli/cli 14800, bob of the team is only subscribed, and
    // the edit adds nothing
    let recorded = events(&alice, &number).await;
    assert_eq!(
        subscriptions(&recorded),
        [
            ("mentioned", "carol"),
            ("subscribed", "carol"),
            ("subscribed", "bob"),
            ("mentioned", "carol"),
            ("subscribed", "carol"),
            ("mentioned", "carol"),
            ("subscribed", "carol"),
            ("mentioned", "alice"),
            ("subscribed", "alice"),
        ]
    );
    for event in named(&recorded, "mentioned")
        .into_iter()
        .chain(named(&recorded, "subscribed"))
    {
        assert_eq!(keys(event), keys_with(&[]));
        assert_eq!(event["commit_id"], Value::Null);
    }
    assert_eq!(prefix(named(&recorded, "mentioned")[0]), Some("MEE"));
    assert_eq!(prefix(named(&recorded, "subscribed")[0]), Some("SE"));
}

#[tokio::test]
async fn a_team_mention_reaches_child_teams_and_only_readers_as_the_docs_say() {
    // Given a child team of the reviewers with alice, and a secret team with alice
    let server = server_with(&[
        json!({"kind": "team", "org": "acme", "name": "Core", "parent": "reviewers",
            "members": [{"login": "alice"}]}),
        json!({"kind": "team", "org": "acme", "name": "Hidden", "privacy": "secret",
            "members": [{"login": "alice"}]}),
    ])
    .await;
    let alice = alice(&server);
    let bob = GoGithub::enterprise(&server).with_token(BOB_TOKEN);
    let carol = GoGithub::enterprise(&server).with_token(CAROL_TOKEN);
    let opened = alice
        .post(&format!("{REPO}/issues"), &json!({"title": "A fault"}))
        .await;
    assert_eq!(opened.status, StatusCode::CREATED, "{}", opened.json());
    let number = opened.json()["number"].clone();
    // When alice names the reviewers, bob the secret team, alice the secret team, and carol,
    // who is not in the organization, the reviewers
    for (client, body) in [
        (&alice, "@acme/reviewers"),
        (&bob, "@acme/hidden"),
        (&alice, "@acme/hidden"),
        (&carol, "@acme/reviewers"),
    ] {
        let commented = client
            .post(
                &format!("{REPO}/issues/{number}/comments"),
                &json!({ "body": body }),
            )
            .await;
        assert_eq!(commented.status, StatusCode::CREATED, "{body}");
    }
    // Then the reviewers and the child team are subscribed, and the secret team only for the
    // owner who names it
    let recorded = events(&alice, &number).await;
    assert_eq!(
        subscriptions(&recorded),
        [
            ("subscribed", "bob"),
            ("subscribed", "alice"),
            ("subscribed", "alice"),
        ]
    );
    // When an issue in the private repository names bob, who cannot read it, and carol
    let private = "repos/acme/private";
    let opened = alice
        .post(
            &format!("{private}/issues"),
            &json!({"title": "A fault", "body": "@bob and @carol"}),
        )
        .await;
    assert_eq!(opened.status, StatusCode::CREATED, "{}", opened.json());
    let number = opened.json()["number"].clone();
    // Then only carol gets the events
    let recorded = events_in(&alice, private, &number).await;
    assert_eq!(
        subscriptions(&recorded),
        [("mentioned", "carol"), ("subscribed", "carol")]
    );
}

#[tokio::test]
async fn one_text_subscribes_each_user_once_and_a_quiet_team_reaches_no_one() {
    // Given a team with notifications off that holds alice, and a second team that holds bob
    let server = server_with(&[
        json!({"kind": "team", "org": "acme", "name": "Quiet", "notifications_enabled": false,
            "members": [{"login": "alice"}]}),
        json!({"kind": "team", "org": "acme", "name": "Pairs", "members": [{"login": "bob"}]}),
    ])
    .await;
    let alice = alice(&server);
    let opened = alice
        .post(&format!("{REPO}/issues"), &json!({"title": "A fault"}))
        .await;
    assert_eq!(opened.status, StatusCode::CREATED, "{}", opened.json());
    let number = opened.json()["number"].clone();
    // When one comment names bob in two cases, his two teams and the quiet team, and an edit
    // changes only the case of a login
    let commented = alice
        .post(
            &format!("{REPO}/issues/{number}/comments"),
            &json!({ "body": "@bob @BOB @acme/reviewers @acme/Pairs @acme/quiet" }),
        )
        .await;
    assert_eq!(
        commented.status,
        StatusCode::CREATED,
        "{}",
        commented.json()
    );
    let id = commented.json()["id"].clone();
    let edited = alice
        .call_json(
            Method::PATCH,
            &format!("{REPO}/issues/comments/{id}"),
            &json!({ "body": "@Bob @acme/reviewers @acme/Pairs @acme/quiet" }),
        )
        .await;
    assert_eq!(edited.status, StatusCode::OK, "{}", edited.json());
    // Then bob is mentioned and subscribed once, the quiet team subscribes no one, and the
    // edit adds no mention, since logins and slugs match without case
    let recorded = events(&alice, &number).await;
    assert_eq!(
        subscriptions(&recorded),
        [("mentioned", "bob"), ("subscribed", "bob")]
    );
}
