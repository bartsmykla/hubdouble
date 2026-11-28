use std::collections::BTreeMap;

use http::Method;
use hubdouble_model::{ApiError, Clock, Timestamp, Token, TokenKind, Visibility};
use hubdouble_store::{Accounts, ActivityEvents, Repos, Tokens};
use hubdouble_testkit::{Answer, GoGithub, TestServer, fixture};
use serde_json::{Value, json};

const ALICE_APP_TOKEN: &str = "ghu_alice_without_organization_events";
const ALICE_OAUTH_TOKEN: &str = "gho_alice_without_sso";
const ALICE_PUBLIC_REPO_TOKEN: &str = "ghp_alice_with_only_public_repo";
const DAY: i64 = 24 * 60 * 60;

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
            for (token, kind, app_id, scopes) in [
                (
                    ALICE_APP_TOKEN,
                    TokenKind::User,
                    Some(fixture::APP_ID),
                    vec![],
                ),
                (
                    ALICE_PUBLIC_REPO_TOKEN,
                    TokenKind::Personal,
                    None,
                    vec!["public_repo".to_owned()],
                ),
            ] {
                let id = tx.next_id();
                tx.table_mut::<Tokens>().insert(Token {
                    id,
                    token: token.into(),
                    kind,
                    user_id: Some(fixture::ALICE_ID),
                    app_id,
                    installation_id: None,
                    repo_ids: None,
                    permissions: BTreeMap::new(),
                    scopes,
                    expires_at: None,
                    revoked: false,
                    owner_seeded: true,
                    created_at: Timestamp::SEED,
                })?;
            }
            Ok(())
        })
        .unwrap_or_else(|error| panic!("{error}"));
    server
}

fn in_repo(events: &[Value], name: &str) -> usize {
    events.iter().filter(|e| e["repo"]["name"] == name).count()
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

#[tokio::test]
async fn the_org_dashboard_needs_authentication_but_not_membership() {
    // Given carol, who is not a member of acme
    let server = start().await;
    act(&server).await;
    let carol = GoGithub::new(&server).with_token(fixture::CAROL_TOKEN);

    // When an anonymous caller and carol read their acme dashboards
    let anonymous = GoGithub::new(&server)
        .get("users/alice/events/orgs/acme")
        .await;
    let outsider = carol.get("users/carol/events/orgs/acme").await;

    // Then the anonymous caller is 401, and carol gets the org's public events, as GitHub serves
    assert_eq!(anonymous.status.as_u16(), 401, "{:?}", anonymous.body);
    let events = ok(&outsider, "carol's dashboard");
    assert!(in_repo(events.as_array().map_or(&[], Vec::as_slice), "acme/public") > 0);
}

#[tokio::test]
async fn a_private_event_shows_only_to_a_token_that_still_reaches_its_repository() {
    // Given alice opened an issue in the private repository
    let server = start().await;
    act(&server).await;

    // When she reads her feeds with a token without the repo scope, and with a user token of an
    // App whose installation does not cover the repository
    let mut feeds = Vec::new();
    for token in [ALICE_PUBLIC_REPO_TOKEN, ALICE_APP_TOKEN] {
        let alice = GoGithub::new(&server).with_token(token);
        feeds.push((token, feed(&alice, "users/alice/events").await));
    }
    let alice = GoGithub::new(&server).with_token(ALICE_PUBLIC_REPO_TOKEN);
    feeds.push((
        "dashboard",
        feed(&alice, "users/alice/events/orgs/acme").await,
    ));

    // Then no feed shows the private event, and the public events stay
    for (what, events) in feeds {
        assert_eq!(in_repo(&events, "acme/private"), 0, "{what}");
        assert!(in_repo(&events, "acme/public") > 0, "{what}");
    }
}

#[tokio::test]
async fn a_private_event_of_an_sso_org_shows_only_after_the_token_is_sso_authorized() {
    // Given alice opened an issue in the private repository of acme, which then enforces SSO,
    // and alice holds an OAuth user token without an SSO authorization
    let server = start().await;
    act(&server).await;
    let store = &server.server.services().store;
    store
        .update(|tx| {
            let mut org = (**tx.table::<Accounts>().get(fixture::ORG_ID).expect("acme")).clone();
            org.sso_enforced = true;
            tx.table_mut::<Accounts>().replace(org)?;
            let id = tx.next_id();
            tx.table_mut::<Tokens>().insert(Token {
                id,
                token: ALICE_OAUTH_TOKEN.into(),
                kind: TokenKind::User,
                user_id: Some(fixture::ALICE_ID),
                app_id: None,
                installation_id: None,
                repo_ids: None,
                permissions: BTreeMap::new(),
                scopes: vec!["repo".to_owned()],
                expires_at: None,
                revoked: false,
                owner_seeded: true,
                created_at: Timestamp::SEED,
            })?;
            Ok(())
        })
        .unwrap_or_else(|error| panic!("{error}"));
    let alice = GoGithub::new(&server).with_token(ALICE_OAUTH_TOKEN);

    // When she reads her feed and the repository's feed
    let before = feed(&alice, "users/alice/events").await;
    let repository = alice.get("repos/acme/private/events").await;

    // Then neither shows the private event: her feed leaves it out and the repository answers 403
    assert_eq!(in_repo(&before, "acme/private"), 0);
    assert!(in_repo(&before, "acme/public") > 0);
    assert_eq!(repository.status.as_u16(), 403, "{:?}", repository.body);

    // When the token is authorized for acme's SSO
    store
        .update(|tx| hubdouble_auth::record_sso(tx, fixture::ALICE_ID, fixture::ORG_ID))
        .unwrap_or_else(|error| panic!("{error}"));

    // Then her feed shows the private event
    let after = feed(&alice, "users/alice/events").await;
    assert!(in_repo(&after, "acme/private") > 0);
}

#[tokio::test]
async fn the_events_of_a_repository_made_private_leave_the_public_feeds() {
    // Given alice acted in the public repository, which then becomes private
    let server = start().await;
    act(&server).await;
    server
        .server
        .services()
        .store
        .update(|tx| {
            let mut repo = (**tx
                .table::<Repos>()
                .get(fixture::PUBLIC_REPO_ID)
                .ok_or_else(ApiError::not_found)?)
            .clone();
            repo.visibility = Visibility::Private;
            tx.table_mut::<Repos>().replace(repo)?;
            Ok(())
        })
        .unwrap_or_else(|error| panic!("{error}"));
    let alice = GoGithub::new(&server).with_token(fixture::ALICE_TOKEN);
    let anonymous = GoGithub::new(&server);

    // When the public feeds and alice's own feed are read
    let mut public = Vec::new();
    for path in [
        "events",
        "orgs/acme/events",
        "users/alice/events/public",
        "users/alice/events",
    ] {
        public.push((path, feed(&anonymous, path).await));
    }
    let own = feed(&alice, "users/alice/events").await;

    // Then only alice's own feed still shows the repository's events
    for (path, events) in public {
        assert_eq!(in_repo(&events, "acme/public"), 0, "{path}");
    }
    assert!(in_repo(&own, "acme/public") > 0);
}

#[tokio::test]
async fn event_feeds_hold_only_the_past_30_days() {
    // Given events in the public repository
    let server = start().await;
    act(&server).await;
    let anonymous = GoGithub::new(&server);
    let fresh = feed(&anonymous, "repos/acme/public/events").await;

    // When 29 days pass, and then 2 more
    server.clock.advance(29 * DAY);
    let within = feed(&anonymous, "repos/acme/public/events").await;
    server.clock.advance(2 * DAY);
    let past = feed(&anonymous, "repos/acme/public/events").await;

    // Then the events show within 30 days and are gone after
    assert!(fresh.len() >= 4);
    assert_eq!(within, fresh);
    assert_eq!(past, Vec::<Value>::new());
}

#[tokio::test]
async fn a_new_event_drops_the_events_past_30_days_from_the_store() {
    // Given events in the public repository, and 31 days later
    let server = start().await;
    act(&server).await;
    server.clock.advance(31 * DAY);

    // When alice opens another issue
    let alice = GoGithub::new(&server).with_token(fixture::ALICE_TOKEN);
    ok(
        &alice
            .post("repos/acme/public/issues", &json!({"title": "Later"}))
            .await,
        "issue",
    );

    // Then the store keeps only that issue's event
    let kept: Vec<&'static str> = server
        .server
        .services()
        .store
        .view(|tx| {
            Ok(tx
                .table::<ActivityEvents>()
                .iter()
                .map(|e| e.kind)
                .collect())
        })
        .unwrap_or_else(|error: ApiError| panic!("{error}"));
    assert_eq!(kept, ["IssuesEvent"]);
}

#[tokio::test]
async fn an_event_past_30_days_is_dropped_behind_a_newer_one_after_the_clock_moved_back() {
    // Given an issue opened 40 days ahead, then the clock moved back and a second issue opened
    let server = start().await;
    let alice = GoGithub::new(&server).with_token(fixture::ALICE_TOKEN);
    let start = server.clock.now();
    let open = async |title: &str| {
        ok(
            &alice
                .post("repos/acme/public/issues", &json!({"title": title}))
                .await,
            "issue",
        );
    };
    server.clock.set(start.plus_seconds(40 * DAY));
    open("Ahead").await;
    server.clock.set(start);
    open("Behind").await;
    server.clock.set(start.plus_seconds(35 * DAY));

    // When alice opens a third issue, when only the second is past 30 days
    open("Later").await;

    // Then the store drops the second issue's event and keeps the other two
    let kept: Vec<String> = server
        .server
        .services()
        .store
        .view(|tx| {
            Ok(tx
                .table::<ActivityEvents>()
                .iter()
                .map(|e| {
                    e.payload["issue"]["title"]
                        .as_str()
                        .unwrap_or_default()
                        .to_owned()
                })
                .collect())
        })
        .unwrap_or_else(|error: ApiError| panic!("{error}"));
    assert_eq!(kept, ["Ahead", "Later"]);
}
