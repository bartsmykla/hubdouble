use std::collections::BTreeMap;

use http::Method;
use hubdouble_model::{Timestamp, Token, TokenKind};
use hubdouble_store::Tokens;
use hubdouble_testkit::{Answer, GoGithub, TestServer, fixture};
use serde_json::{Value, json};

const ALICE_APP_TOKEN: &str = "ghu_alice_without_organization_events";

async fn start() -> TestServer {
    let server = TestServer::builder()
        .frame(hubdouble_api::register_all)
        .fixture()
        .start()
        .await;
    server
        .server
        .services()
        .store
        .update(|tx| {
            let id = tx.next_id();
            tx.table_mut::<Tokens>().insert(Token {
                id,
                token: ALICE_APP_TOKEN.into(),
                kind: TokenKind::User,
                user_id: Some(fixture::ALICE_ID),
                app_id: Some(fixture::APP_ID),
                installation_id: None,
                repo_ids: None,
                permissions: BTreeMap::new(),
                scopes: Vec::new(),
                expires_at: None,
                revoked: false,
                owner_seeded: true,
                created_at: Timestamp::SEED,
            })?;
            Ok(())
        })
        .unwrap_or_else(|error| panic!("{error}"));
    server
}

fn ok(answer: &Answer, what: &str) -> Value {
    assert!(
        answer.status.is_success(),
        "{what}: {} {:?}",
        answer.status,
        answer.body
    );
    answer.json()
}

async fn feed(client: &GoGithub, path: &str) -> Vec<Value> {
    let body = ok(&client.get(path).await, path);
    body.as_array().cloned().unwrap_or_default()
}

fn types(events: &[Value]) -> Vec<&str> {
    events.iter().filter_map(|e| e["type"].as_str()).collect()
}

fn ids(events: &[Value]) -> Vec<&str> {
    events.iter().filter_map(|e| e["id"].as_str()).collect()
}

async fn act(server: &TestServer) -> (Value, Value) {
    let alice = GoGithub::new(server).with_token(fixture::ALICE_TOKEN);
    let main = ok(
        &alice.get("repos/acme/public/git/ref/heads/main").await,
        "main",
    );
    ok(
        &alice
            .post(
                "repos/acme/public/git/refs",
                &json!({"ref": "refs/heads/topic", "sha": main["object"]["sha"]}),
            )
            .await,
        "push",
    );
    let issue = ok(
        &alice
            .post("repos/acme/public/issues", &json!({"title": "Track it"}))
            .await,
        "issue",
    );
    let pull = ok(
        &alice
            .post(
                "repos/acme/public/pulls",
                &json!({"title": "Ship it", "head": "feature", "base": "main"}),
            )
            .await,
        "pull",
    );
    ok(
        &alice
            .post("repos/acme/private/issues", &json!({"title": "Secret"}))
            .await,
        "private issue",
    );
    (issue, pull)
}

#[tokio::test]
async fn a_push_an_issue_and_a_pull_request_reach_the_repository_user_and_org_feeds() {
    // Given alice pushes a branch, opens an issue and a pull request in the public repository
    let server = start().await;
    let (issue, pull) = act(&server).await;
    let alice = GoGithub::new(&server).with_token(fixture::ALICE_TOKEN);
    let anonymous = GoGithub::new(&server);

    // When the repository's events are read
    let events = feed(&anonymous, "repos/acme/public/events").await;

    // Then they are newest first, with the event types and payloads of the events API
    assert_eq!(
        types(&events)[..4],
        [
            "PullRequestEvent",
            "IssuesEvent",
            "CreateEvent",
            "PushEvent"
        ]
    );
    let [pr, opened, create, push] = &events[..4] else {
        panic!("{events:?}");
    };
    assert_eq!(pr["payload"]["action"], "opened");
    assert_eq!(pr["payload"]["number"], pull["number"]);
    assert_eq!(pr["payload"]["pull_request"]["head"]["ref"], "feature");
    assert_eq!(opened["payload"]["issue"]["number"], issue["number"]);
    assert_eq!(create["payload"]["ref"], "topic");
    assert_eq!(create["payload"]["full_ref"], "refs/heads/topic");
    assert_eq!(push["payload"]["ref"], "refs/heads/topic");
    assert_eq!(push["payload"]["repository_id"], fixture::PUBLIC_REPO_ID);
    for event in &events[..4] {
        assert_eq!(event["actor"]["login"], "alice");
        assert_eq!(event["repo"]["name"], "acme/public");
        assert_eq!(event["org"]["login"], "acme");
        assert_eq!(event["public"], true);
    }

    // And the same events reach alice's, the org's, the network's and the public feeds
    let newest: Vec<&str> = ids(&events)[..4].to_vec();
    for path in [
        "users/alice/events/public",
        "orgs/acme/events",
        "networks/acme/public/events",
        "events",
    ] {
        let other = feed(&anonymous, path).await;
        let other_ids = ids(&other);
        assert!(
            newest.iter().all(|id| other_ids.contains(id)),
            "{path}: {other_ids:?}"
        );
    }
    let dashboard = feed(&alice, "users/alice/events/orgs/acme").await;
    assert!(ids(&dashboard).contains(&newest[0]));
}

#[tokio::test]
async fn private_repository_events_show_only_to_the_user_who_can_see_them() {
    // Given alice opened an issue in the private repository
    let server = start().await;
    act(&server).await;
    let alice = GoGithub::new(&server).with_token(fixture::ALICE_TOKEN);
    let bob = GoGithub::new(&server).with_token(fixture::BOB_TOKEN);
    let anonymous = GoGithub::new(&server);
    let private = |events: &[Value]| {
        events
            .iter()
            .filter(|e| e["repo"]["name"] == "acme/private")
            .count()
    };

    // When alice reads her own feeds, and others read the public ones
    let own = feed(&alice, "users/alice/events").await;
    let dashboard = feed(&alice, "users/alice/events/orgs/acme").await;
    let seen_by_bob = feed(&bob, "users/alice/events").await;
    let own_public = feed(&alice, "users/alice/events/public").await;

    // Then only alice's own feeds show the private event, marked as not public
    assert!(private(&own) > 0);
    assert!(private(&dashboard) > 0);
    let newest = own.first().cloned().unwrap_or_default();
    assert_eq!(newest["type"], "IssuesEvent");
    assert_eq!(newest["repo"]["name"], "acme/private");
    assert_eq!(newest["public"], false);
    for (path, events) in [
        ("bob reads alice's events", seen_by_bob),
        ("alice's public events", own_public),
        ("public events", feed(&anonymous, "events").await),
        ("org events", feed(&anonymous, "orgs/acme/events").await),
    ] {
        assert_eq!(private(&events), 0, "{path}");
    }

    // And the private repository's feed and another user's dashboard answer 404
    for (client, path) in [
        (&anonymous, "repos/acme/private/events"),
        (&anonymous, "networks/acme/private/events"),
        (&bob, "users/alice/events/orgs/acme"),
    ] {
        assert_eq!(client.get(path).await.status.as_u16(), 404, "{path}");
    }
}

#[tokio::test]
async fn a_collaborator_receives_the_events_of_a_repository_it_can_push_to() {
    // Given carol can push to the public repository, and alice acts in it
    let server = start().await;
    let answer = server
        .admin(
            Method::PUT,
            "repository/acme/public/collaborators/carol",
            Some(&json!({"permission": "write"})),
        )
        .await;
    assert_eq!(answer.status.as_u16(), 204, "{:?}", answer.body);
    act(&server).await;
    let carol = GoGithub::new(&server).with_token(fixture::CAROL_TOKEN);
    let anonymous = GoGithub::new(&server);

    // When carol's received events are read
    let received = feed(&carol, "users/carol/received_events").await;
    let received_public = feed(&anonymous, "users/carol/received_events/public").await;

    // Then they hold alice's events in that repository, and bob, without a role, gets none
    assert_eq!(
        types(&received)[..4],
        [
            "PullRequestEvent",
            "IssuesEvent",
            "CreateEvent",
            "PushEvent"
        ]
    );
    assert_eq!(ids(&received_public), ids(&received));
    assert_eq!(
        feed(&anonymous, "users/bob/received_events").await,
        Vec::<Value>::new()
    );
}

#[tokio::test]
async fn event_feeds_page_newest_first_and_stop_at_300_events() {
    // Given four events in the public repository
    let server = start().await;
    act(&server).await;
    let anonymous = GoGithub::new(&server);
    let all = feed(&anonymous, "repos/acme/public/events").await;

    // When the feed is read two at a time
    let first = anonymous.get("repos/acme/public/events?per_page=2").await;
    let second = feed(&anonymous, "repos/acme/public/events?per_page=2&page=2").await;

    // Then the pages follow each other with a Link header
    let first_page = ok(&first, "first page");
    assert!(first.header("link").contains("rel=\"next\""));
    let paged: Vec<Value> = first_page
        .as_array()
        .into_iter()
        .flatten()
        .chain(&second)
        .cloned()
        .collect();
    assert_eq!(ids(&paged), ids(&all)[..4]);

    // And a page past the 300th event answers 422, as GitHub limits the timeline
    let past = anonymous.get("events?per_page=100&page=4").await;
    assert_eq!(past.status.as_u16(), 422, "{:?}", past.body);
    ok(&anonymous.get("events?per_page=100&page=3").await, "page 3");
}

#[tokio::test]
async fn the_org_dashboard_needs_the_organization_events_permission() {
    // Given an App user token whose App lacks organization events
    let server = start().await;
    let client = GoGithub::new(&server).with_token(ALICE_APP_TOKEN);

    // When it reads alice's org dashboard
    let answer = client.get("users/alice/events/orgs/acme").await;

    // Then GitHub answers 403 and names the permission
    assert_eq!(answer.status.as_u16(), 403, "{:?}", answer.body);
    assert!(
        answer
            .header("x-accepted-github-permissions")
            .contains("organization_events=read")
    );
}
