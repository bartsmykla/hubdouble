use std::sync::Arc;

use http::header::{ACCEPT, HeaderValue};
use http::{HeaderMap, Method, StatusCode};
use hubdouble_git::{EntryMode, FileChange, ObjectId, Signature};
use hubdouble_model::{Access, Clock, Issue, IssueState, Pull, PullRef, Token, TokenKind, node_id};
use hubdouble_store::{Issues, Releases, Tokens};
use hubdouble_testkit::fixture::{
    ALICE_ID, ALICE_TOKEN, APP_ID, APP_PUBLIC_KEY, BOB_TOKEN, CAROL_TOKEN, INSTALLATION_ID,
    PUBLIC_REPO_ID, WEBHOOK_SECRET, WEBHOOK_URL_ENV,
};
use hubdouble_testkit::{Answer, Delivery, GoGithub, Graphql, TestServer, validate};
use serde_json::{Value, json};

const OCTET_STREAM: &str = "application/octet-stream";

async fn start(entries: Vec<Value>) -> TestServer {
    entries
        .into_iter()
        .fold(
            TestServer::builder()
                .register(hubdouble_api_repos::register)
                .fixture(),
            hubdouble_testkit::Builder::entry,
        )
        .start()
        .await
}

fn alice(server: &TestServer) -> GoGithub {
    GoGithub::new(server).with_token(ALICE_TOKEN)
}

fn valid(operation_id: &str, answer: &Answer) -> Value {
    serde_json::from_slice(&answer.body)
        .unwrap_or_else(|e| panic!("{operation_id} {}: not JSON: {e}", answer.status))
}

fn installation_token(server: &TestServer, token: &str, permissions: &[(&str, Access)]) {
    let now = server.clock.now();
    server
        .server
        .services()
        .store
        .update(|tx| {
            let id = tx.next_id();
            tx.table_mut::<Tokens>().insert(Token {
                id,
                token: token.to_owned(),
                kind: TokenKind::Installation,
                app_id: Some(APP_ID),
                installation_id: Some(INSTALLATION_ID),
                user_id: None,
                repo_ids: None,
                permissions: permissions
                    .iter()
                    .map(|(name, access)| ((*name).to_owned(), *access))
                    .collect(),
                scopes: Vec::new(),
                expires_at: None,
                revoked: false,
                owner_seeded: false,
                created_at: now,
            })?;
            Ok(())
        })
        .unwrap_or_else(|e| panic!("insert the token: {e:?}"));
}

fn watcher() -> [Value; 2] {
    [
        json!({"kind": "app", "id": 4002, "slug": "watcher", "owner": "acme",
               "public_key": APP_PUBLIC_KEY,
               "webhook": {"url": format!("{{{{env \"{WEBHOOK_URL_ENV}\"}}}}"),
                           "secret": WEBHOOK_SECRET},
               "permissions": {"contents": "read", "metadata": "read"},
               "events": ["release", "repository"]}),
        json!({"kind": "installation", "id": 5002, "app": "watcher", "account": "acme",
               "repository_selection": "all"}),
    ]
}

async fn deliveries(server: &TestServer, count: usize) -> Vec<Delivery> {
    let drained = server.admin(Method::POST, "hooks/drain", None).await;
    assert_eq!(drained.status, StatusCode::NO_CONTENT);
    let received = server.receiver.received().await;
    assert_eq!(received.len(), count);
    received
}

fn delivered(deliveries: &[Delivery], event: &str, action: &str) -> Value {
    delivered_as(deliveries, event, Some(action))
}

fn delivered_as(deliveries: &[Delivery], event: &str, action: Option<&str>) -> Value {
    let found = deliveries
        .iter()
        .find(|d| d.header("x-github-event") == event && d.json()["action"] == json!(action))
        .unwrap_or_else(|| {
            let got: Vec<(String, Value)> = deliveries
                .iter()
                .map(|d| {
                    (
                        d.header("x-github-event").to_owned(),
                        d.json()["action"].clone(),
                    )
                })
                .collect();
            panic!("no {event}.{action:?} delivery in {got:?}")
        });
    let payload = found.json();
    if event == "release" {
        let release = &payload["release"];
        if let Err(e) = validate::openapi_response("repos/get-release-by-tag", 200, release) {
            panic!("{event}.{action:?} release: {e:?}");
        }
    }
    if let Err(e) = validate::webhook_payload(event, action, &payload) {
        panic!("{event}.{action:?}: {e:?}");
    }
    payload
}

#[tokio::test]
async fn a_repository_answers_by_name_and_id_and_a_hidden_one_is_not_found() {
    // Given
    let server = start(Vec::new()).await;
    installation_token(
        &server,
        "ghs_1000000000000000000000000000000000a1",
        &[("contents", Access::Read)],
    );
    let installation =
        GoGithub::new(&server).with_token("ghs_1000000000000000000000000000000000a1");
    // When
    let by_name = alice(&server).get("repos/acme/public").await;
    let by_id = alice(&server)
        .get(&format!("repositories/{PUBLIC_REPO_ID}"))
        .await;
    let anonymous = GoGithub::new(&server).get("repos/acme/private").await;
    let uncovered = installation.get("repos/acme/private").await;
    let covered = installation.get("repos/acme/public").await;
    // Then
    assert_eq!(by_name.status, StatusCode::OK);
    let repo = valid("repos/get", &by_name);
    assert_eq!(repo["full_name"], json!("acme/public"));
    assert_eq!(repo["owner"]["login"], json!("acme"));
    assert_eq!(repo["organization"]["login"], json!("acme"));
    assert_eq!(repo["permissions"]["admin"], json!(true));
    assert_eq!(by_id.status, StatusCode::OK);
    assert_eq!(valid("repos/get", &by_id)["node_id"], repo["node_id"]);
    assert_eq!(anonymous.status, StatusCode::NOT_FOUND);
    assert_eq!(uncovered.status, StatusCode::NOT_FOUND);
    assert_eq!(covered.status, StatusCode::OK);
}

#[tokio::test]
async fn org_repositories_page_and_filter_by_type() {
    // Given
    let server = start(Vec::new()).await;
    // When
    let first = alice(&server).get("orgs/acme/repos?per_page=1").await;
    let private = alice(&server).get("orgs/acme/repos?type=private").await;
    let by_name = alice(&server).get("orgs/acme/repos?sort=full_name").await;
    // Then
    assert_eq!(first.status, StatusCode::OK);
    assert_eq!(
        valid("repos/list-for-org", &first)
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert!(first.header("link").contains("rel=\"next\""));
    let names = |a: &Answer| -> Vec<Value> {
        a.json()
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["name"].clone())
            .collect()
    };
    assert_eq!(names(&private), vec![json!("private")]);
    assert_eq!(names(&by_name), vec![json!("private"), json!("public")]);
}

fn release_body() -> Value {
    json!({"id": 7001, "repository": "acme/public", "tag_name": "v2.0.0",
           "name": "Two", "body": "notes", "author": "alice",
           "assets": [{"id": 7101, "name": "prompts.tar", "content_type": "application/x-tar",
                       "content": "the asset bytes"}]})
}

fn release_seed() -> Value {
    let mut seed = release_body();
    seed["kind"] = json!("release");
    seed
}

#[tokio::test]
async fn release_list_renders_only_the_requested_page() {
    let server = start(vec![
        release_seed(),
        json!({"kind": "release", "id": 7002, "repository": "acme/public",
               "tag_name": "v3.0.0"}),
    ])
    .await;
    server
        .server
        .services()
        .store
        .update(|tx| {
            let mut off_page = tx.table::<Releases>().get(7001).unwrap().as_ref().clone();
            off_page.author_id = i64::MAX;
            tx.table_mut::<Releases>().replace(off_page)?;
            Ok(())
        })
        .unwrap();
    let first = alice(&server)
        .get("repos/acme/public/releases?per_page=1")
        .await;
    assert_eq!(first.status, StatusCode::OK);
    assert_eq!(first.json()[0]["id"], 7002);
    assert!(first.header("link").contains("page=2>; rel=\"next\""));
}

#[tokio::test]
async fn releases_list_by_tag_and_hide_drafts_from_readers() {
    // Given
    let server = start(vec![
        release_seed(),
        json!({"kind": "release", "id": 7002, "repository": "acme/public",
               "tag_name": "v3.0.0", "draft": true}),
    ])
    .await;
    installation_token(
        &server,
        "ghs_2000000000000000000000000000000000a2",
        &[("contents", Access::Read), ("metadata", Access::Read)],
    );
    installation_token(
        &server,
        "ghs_3000000000000000000000000000000000a3",
        &[("metadata", Access::Read)],
    );
    let reader = GoGithub::new(&server).with_token("ghs_2000000000000000000000000000000000a2");
    let metadata_only =
        GoGithub::new(&server).with_token("ghs_3000000000000000000000000000000000a3");
    // When
    let as_admin = alice(&server).get("repos/acme/public/releases").await;
    let as_reader = reader.get("repos/acme/public/releases?per_page=100").await;
    let denied = metadata_only.get("repos/acme/public/releases").await;
    let by_tag = reader.get("repos/acme/public/releases/tags/v2.0.0").await;
    let draft_tag = alice(&server)
        .get("repos/acme/public/releases/tags/v3.0.0")
        .await;
    // Then
    let ids = |v: &Value| -> Vec<Value> {
        v.as_array()
            .unwrap()
            .iter()
            .map(|r| r["id"].clone())
            .collect()
    };
    let mut admin_ids = ids(&valid("repos/list-releases", &as_admin));
    admin_ids.sort_by_key(Value::as_i64);
    assert_eq!(admin_ids, vec![json!(7001), json!(7002)]);
    assert_eq!(
        ids(&valid("repos/list-releases", &as_reader)),
        vec![json!(7001)]
    );
    assert_eq!(denied.status, StatusCode::FORBIDDEN);
    assert_eq!(by_tag.status, StatusCode::OK);
    let release = valid("repos/get-release-by-tag", &by_tag);
    assert_eq!(release["author"]["login"], json!("alice"));
    assert_eq!(release["assets"][0]["id"], json!(7101));
    assert_eq!(release["assets"][0]["size"], json!(15));
    assert_eq!(draft_tag.status, StatusCode::NOT_FOUND);
    let tag = server
        .server
        .services()
        .store
        .view(|tx| {
            Ok(server
                .server
                .services()
                .git
                .resolve_ref(tx, PUBLIC_REPO_ID, "refs/tags/v2.0.0")
                .is_ok())
        })
        .unwrap();
    assert!(tag);
}

#[tokio::test]
async fn an_asset_asked_for_as_octet_stream_redirects_to_its_bytes() {
    // Given
    let server = start(vec![release_seed()]).await;
    let path = "repos/acme/public/releases/assets/7101";
    // When
    let metadata = alice(&server).get(path).await;
    let redirect = alice(&server)
        .call_headers(
            Method::GET,
            path,
            HeaderMap::from_iter([(ACCEPT, HeaderValue::from_static(OCTET_STREAM))]),
        )
        .await;
    let landed = redirect
        .header("location")
        .strip_prefix(&server.base)
        .unwrap()
        .to_owned();
    let bytes = GoGithub::new(&server).get(&landed).await.body;
    let log = server.admin(Method::GET, "log", None).await.json();
    // Then
    let asset = valid("repos/get-release-asset", &metadata);
    assert_eq!(asset["name"], json!("prompts.tar"));
    assert_eq!(asset["content_type"], json!("application/x-tar"));
    assert!(landed.starts_with("/_blobs/"), "landed on {landed}");
    assert_eq!(&bytes[..], b"the asset bytes");
    let statuses: Vec<&Value> = log["entries"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["operation_id"] == json!("repos/get-release-asset"))
        .map(|e| &e["status"])
        .collect();
    assert_eq!(statuses, vec![&json!(200), &json!(302)]);
}

#[tokio::test]
async fn a_private_asset_reaches_its_bytes_only_through_the_checked_redirect() {
    // Given
    let mut seed = release_seed();
    seed["repository"] = json!("acme/private");
    let server = start(vec![seed]).await;
    let path = "repos/acme/private/releases/assets/7101";
    // When
    let redirect = alice(&server)
        .call_headers(
            Method::GET,
            path,
            HeaderMap::from_iter([(ACCEPT, HeaderValue::from_static(OCTET_STREAM))]),
        )
        .await;
    let landed = redirect
        .header("location")
        .strip_prefix(&server.base)
        .unwrap()
        .to_owned();
    let anonymous = GoGithub::new(&server);
    let checked = anonymous.get(&landed).await;
    let guessed = anonymous.get("/_blobs/release-asset-7101").await;
    let unchecked = anonymous
        .call_headers(
            Method::GET,
            path,
            HeaderMap::from_iter([(ACCEPT, HeaderValue::from_static(OCTET_STREAM))]),
        )
        .await;
    // Then
    assert_eq!(redirect.status, StatusCode::FOUND);
    assert_eq!(redirect.header("cache-control"), "");
    assert_eq!(
        redirect.header("vary"),
        "Accept-Encoding, Accept, X-Requested-With"
    );
    assert_eq!(&checked.body[..], b"the asset bytes");
    assert!(!landed.contains("7101"), "landed on {landed}");
    assert_eq!(guessed.status, StatusCode::NOT_FOUND);
    assert_eq!(unchecked.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn an_asset_link_answers_as_the_release_asset_cdn_after_five_minutes() {
    // Given the download redirect of an asset
    let server = start(vec![release_seed()]).await;
    let redirect = alice(&server)
        .call_headers(
            Method::GET,
            "repos/acme/public/releases/assets/7101",
            HeaderMap::from_iter([(ACCEPT, HeaderValue::from_static(OCTET_STREAM))]),
        )
        .await;
    assert_eq!(redirect.status, StatusCode::FOUND);
    let path = redirect
        .header("location")
        .strip_prefix(&server.base)
        .unwrap()
        .to_owned();
    let (bare, _) = path.split_once('?').unwrap();
    let root = GoGithub::new(&server);
    // When the link is read 300 seconds after the redirect, as the live cli/cli asset 761188502 link's jwt has nbf to exp of 300 seconds
    server.clock.advance(300);
    let last = root.get(&path).await;
    // And a second later, and without its signature
    server.clock.advance(1);
    let expired = root.get(&path).await;
    let unsigned = root.get(bare).await;
    // Then the link answers the bytes until it expires, and then the CDN's 618, as live links answered on 2026-11-25
    assert_eq!(last.status, StatusCode::OK);
    assert_eq!(&last.body[..], b"the asset bytes");
    for (answer, content_type) in [
        (&expired, OCTET_STREAM),
        (&unsigned, "text/html; charset=utf-8"),
    ] {
        assert_eq!(answer.status.as_u16(), 618);
        assert_eq!(answer.header("content-type"), content_type);
    }
}

#[tokio::test]
async fn an_asset_link_refuses_a_missing_part_as_the_cdn_refuses_its_token() {
    // Given the download redirect of an asset
    let server = start(vec![release_seed()]).await;
    let redirect = alice(&server)
        .call_headers(
            Method::GET,
            "repos/acme/public/releases/assets/7101",
            HeaderMap::from_iter([(ACCEPT, HeaderValue::from_static(OCTET_STREAM))]),
        )
        .await;
    assert_eq!(redirect.status, StatusCode::FOUND);
    let path = redirect
        .header("location")
        .strip_prefix(&server.base)
        .unwrap()
        .to_owned();
    let (bare, query) = path.split_once('?').unwrap();
    let without = |name: &str| {
        let kept: Vec<&str> = query
            .split('&')
            .filter(|p| !p.starts_with(&format!("{name}=")))
            .collect();
        format!("{bare}?{}", kept.join("&"))
    };
    assert!(query.contains("%3A"), "{query}");
    let root = GoGithub::new(&server);
    // When the link is read without sig, without se, and with a lower case %3a
    let unsigned = root.get(&without("sig")).await;
    let undated = root.get(&without("se")).await;
    let lower = root.get(&path.replace("%3A", "%3a")).await;
    // Then a link without sig is a token not provided and a link without se fails its
    // signature, as live cli/cli v2.40.0 asset links answered with jwt dropped and with jwt
    // kept but its exp claim removed on 2026-11-25, and %3a decodes as %3A
    for (answer, code) in [
        (&unsigned, "jwt:jwt-not-provided"),
        (&undated, "jwt:signature-fail"),
    ] {
        assert_eq!(answer.status.as_u16(), 618, "{code}");
        assert_eq!(answer.header("content-type"), OCTET_STREAM, "{code}");
        assert!(
            String::from_utf8_lossy(&answer.body).contains(&format!("<title>618 {code}</title>")),
            "{code}"
        );
    }
    assert_eq!(lower.status, StatusCode::OK);
    assert_eq!(&lower.body[..], b"the asset bytes");
}

fn ruleset(id: i64, include: &[&str], rule: &str, enforcement: &str) -> Value {
    json!({"kind": "ruleset", "id": id, "repository": "acme/public", "name": format!("r{id}"),
           "target": "branch", "enforcement": enforcement,
           "conditions": {"ref_name": {"include": include, "exclude": []}},
           "rules": [{"type": rule}]})
}

#[tokio::test]
async fn branch_rules_list_the_active_matching_rules_in_pages() {
    // Given
    let server = start(vec![
        ruleset(8001, &["~DEFAULT_BRANCH"], "deletion", "active"),
        ruleset(8002, &["refs/heads/main"], "non_fast_forward", "active"),
        ruleset(8003, &["~ALL"], "required_linear_history", "active"),
        ruleset(8004, &["~ALL"], "required_signatures", "disabled"),
        ruleset(8005, &["refs/heads/dev*"], "creation", "active"),
        json!({"kind": "ruleset", "id": 8006, "org": "acme", "name": "org", "target": "branch",
               "enforcement": "active",
               "conditions": {"ref_name": {"include": ["~DEFAULT_BRANCH"], "exclude": []},
                              "repository_name": {"include": ["public"], "exclude": []}},
               "rules": [{"type": "update"}]}),
    ])
    .await;
    // When
    let first = alice(&server)
        .get("repos/acme/public/rules/branches/main?per_page=3&page=1")
        .await;
    let second = alice(&server)
        .get("repos/acme/public/rules/branches/main?per_page=3&page=2")
        .await;
    let develop = alice(&server)
        .get("repos/acme/public/rules/branches/develop?per_page=100")
        .await;
    // Then
    let rules = |v: &Value| -> Vec<(i64, String)> {
        let mut out: Vec<(i64, String)> = v
            .as_array()
            .unwrap()
            .iter()
            .map(|r| {
                (
                    r["ruleset_id"].as_i64().unwrap(),
                    r["type"].as_str().unwrap().to_owned(),
                )
            })
            .collect();
        out.sort();
        out
    };
    let first_rules = rules(&valid("repos/get-branch-rules", &first));
    let second_rules = rules(&valid("repos/get-branch-rules", &second));
    assert_eq!(first_rules.len(), 3);
    assert_eq!(second_rules.len(), 1);
    assert!(first.header("link").contains("page=2"));
    assert!(first.header("link").contains("rel=\"next\""));
    let mut all: Vec<i64> = first_rules
        .iter()
        .chain(&second_rules)
        .map(|(id, _)| *id)
        .collect();
    all.sort_unstable();
    assert_eq!(all, vec![8001, 8002, 8003, 8006]);
    let org_rule = valid("repos/get-branch-rules", &first)
        .as_array()
        .unwrap()
        .iter()
        .chain(second.json().as_array().unwrap())
        .find(|r| r["ruleset_id"] == json!(8006))
        .cloned()
        .unwrap();
    assert_eq!(org_rule["ruleset_source_type"], json!("Organization"));
    assert_eq!(org_rule["ruleset_source"], json!("acme"));
    assert_eq!(
        rules(&valid("repos/get-branch-rules", &develop)),
        vec![
            (8003, "required_linear_history".to_owned()),
            (8005, "creation".to_owned())
        ]
    );
}

#[tokio::test]
async fn a_ruleset_tells_the_caller_whether_it_can_bypass() {
    // Given
    let server = start(vec![
        json!({"kind": "ruleset", "id": 8101, "org": "acme", "name": "release", "target": "tag",
               "enforcement": "active",
               "bypass_actors": [{"actor_type": "OrganizationAdmin", "actor_id": 1,
                                  "bypass_mode": "always"}],
               "conditions": {"ref_name": {"include": ["refs/tags/v*"], "exclude": []},
                              "repository_name": {"include": ["public"], "exclude": []}},
               "rules": [{"type": "creation"}]}),
        json!({"kind": "ruleset", "id": 8102, "repository": "acme/public", "name": "main",
               "target": "branch", "enforcement": "active",
               "bypass_actors": [{"actor_type": "RepositoryRole", "actor_id": 5,
                                  "bypass_mode": "pull_request"}],
               "conditions": {"ref_name": {"include": ["~DEFAULT_BRANCH"], "exclude": []}},
               "rules": [{"type": "deletion"}]}),
    ])
    .await;
    let bob = GoGithub::new(&server).with_token(BOB_TOKEN);
    // When
    let admin_org = alice(&server)
        .get("repos/acme/public/rulesets/8101?includes_parents=true")
        .await;
    let member_org = bob
        .get("repos/acme/public/rulesets/8101?includes_parents=true")
        .await;
    let without_parents = alice(&server)
        .get("repos/acme/public/rulesets/8101?includes_parents=false")
        .await;
    let admin_repo = alice(&server)
        .get("repos/acme/public/rulesets/8102?includes_parents=true")
        .await;
    // Then
    let admin_org = valid("repos/get-repo-ruleset", &admin_org);
    assert_eq!(admin_org["current_user_can_bypass"], json!("always"));
    assert_eq!(admin_org["source_type"], json!("Organization"));
    assert_eq!(admin_org["source"], json!("acme"));
    assert_eq!(
        valid("repos/get-repo-ruleset", &member_org)["current_user_can_bypass"],
        json!("never")
    );
    assert_eq!(without_parents.status, StatusCode::NOT_FOUND);
    assert_eq!(
        valid("repos/get-repo-ruleset", &admin_repo)["current_user_can_bypass"],
        json!("pull_requests_only")
    );
}

fn commit_on(server: &TestServer, branch: &str, parent: ObjectId, path: &str) -> ObjectId {
    let services = server.server.services();
    let author = Signature::seed();
    services
        .store
        .update(|tx| {
            services
                .git
                .commit(
                    tx,
                    PUBLIC_REPO_ID,
                    ALICE_ID,
                    branch,
                    Some(parent),
                    &[FileChange::Write {
                        path: path.to_owned(),
                        data: Arc::from(&b"x\n"[..]),
                        mode: EntryMode::File,
                    }],
                    path,
                    &author,
                    &author,
                    false,
                )
                .map_err(Into::into)
        })
        .unwrap_or_else(|e| panic!("commit on {branch}: {e:?}"))
}

fn head(server: &TestServer, branch: &str) -> ObjectId {
    let services = server.server.services();
    services
        .store
        .view(|tx| {
            services
                .git
                .resolve_ref(tx, PUBLIC_REPO_ID, &format!("refs/heads/{branch}"))
                .map_err(Into::into)
        })
        .unwrap_or_else(|e| panic!("resolve {branch}: {e:?}"))
}

fn move_ref(server: &TestServer, branch: &str, old: Option<ObjectId>, new: Option<ObjectId>) {
    let services = server.server.services();
    services
        .store
        .update(|tx| {
            services
                .git
                .update_ref(
                    tx,
                    PUBLIC_REPO_ID,
                    ALICE_ID,
                    &format!("refs/heads/{branch}"),
                    old,
                    new,
                    true,
                )
                .map_err(Into::into)
        })
        .unwrap_or_else(|e| panic!("move {branch}: {e:?}"));
}

fn merged_pull(server: &TestServer, merge_commit: ObjectId) {
    let now = server.clock.now();
    server
        .server
        .services()
        .store
        .update(|tx| {
            let id = tx.next_id();
            let pull_id = tx.next_id();
            tx.table_mut::<Issues>().insert(Issue {
                id,
                node_id: node_id::entity(node_id::ISSUE, id),
                repo_id: PUBLIC_REPO_ID,
                number: 1,
                title: "merge".to_owned(),
                body: None,
                user_id: ALICE_ID,
                state: IssueState::Closed,
                state_reason: None,
                locked: false,
                active_lock_reason: None,
                label_ids: Vec::new(),
                assignee_ids: Vec::new(),
                closed_by: Some(ALICE_ID),
                created_at: now,
                updated_at: now,
                closed_at: Some(now),
                last_edited_at: None,
                pull: Some(Pull {
                    id: pull_id,
                    node_id: node_id::entity(node_id::PULL_REQUEST, pull_id),
                    head: PullRef {
                        repo_id: Some(PUBLIC_REPO_ID),
                        ref_name: "feature".to_owned(),
                        sha: merge_commit.to_string(),
                    },
                    base: PullRef {
                        repo_id: Some(PUBLIC_REPO_ID),
                        ref_name: "main".to_owned(),
                        sha: merge_commit.to_string(),
                    },
                    draft: false,
                    merged_at: Some(now),
                    merge_commit_sha: Some(merge_commit.to_string()),
                    merged_by: Some(ALICE_ID),
                    mergeable: None,
                    rebaseable: None,
                    maintainer_can_modify: false,
                    requested_reviewers: Vec::new(),
                    requested_teams: Vec::new(),
                }),
            })?;
            Ok(())
        })
        .unwrap_or_else(|e| panic!("insert the pull: {e:?}"));
}

fn activity_types(answer: &Answer) -> Vec<String> {
    match answer.json() {
        Value::Array(items) => items
            .iter()
            .map(|a| a["activity_type"].as_str().unwrap_or_default().to_owned())
            .collect(),
        other => panic!("the activity answer is not a list: {other}"),
    }
}

#[tokio::test]
async fn repository_activity_and_codeowners_require_contents_from_an_installation() {
    let server = start(Vec::new()).await;
    installation_token(
        &server,
        "ghs_repository_metadata_only",
        &[("metadata", Access::Read)],
    );
    let denied = GoGithub::new(&server).with_token("ghs_repository_metadata_only");
    for suffix in ["activity", "codeowners/errors"] {
        let answer = denied.get(&format!("repos/acme/public/{suffix}")).await;
        assert_eq!(answer.status, StatusCode::FORBIDDEN, "{suffix}");
        assert_eq!(
            answer.header("x-accepted-github-permissions"),
            "contents=read"
        );
        let hidden = denied.get(&format!("repos/acme/private/{suffix}")).await;
        assert_eq!(hidden.status, StatusCode::NOT_FOUND, "{suffix}");
    }
}

#[tokio::test]
async fn activity_records_each_branch_change_and_pages_with_cursors() {
    // Given
    let server = start(Vec::new()).await;
    let main = head(&server, "main");
    let feature = head(&server, "feature");
    commit_on(&server, "develop", main, "develop.txt");
    move_ref(&server, "feature", Some(feature), Some(main));
    move_ref(&server, "develop", Some(head(&server, "develop")), None);
    move_ref(&server, "topic", None, Some(main));
    let merge = commit_on(&server, "main", main, "merged.txt");
    merged_pull(&server, merge);
    move_ref(&server, "main", Some(merge), Some(main));
    move_ref(&server, "main", Some(main), Some(merge));
    // When
    let all = alice(&server)
        .get("repos/acme/public/activity?per_page=100")
        .await;
    let main_first = alice(&server)
        .get("repos/acme/public/activity?ref=main&per_page=1")
        .await;
    let next = main_first
        .header("link")
        .split(", ")
        .find(|l| l.ends_with("rel=\"next\""))
        .and_then(|l| l.split("after=").nth(1))
        .and_then(|l| l.split('>').next())
        .unwrap_or_else(|| panic!("no next link in {}", main_first.header("link")))
        .to_owned();
    let main_second = alice(&server)
        .get(&format!(
            "repos/acme/public/activity?ref=main&per_page=1&after={next}"
        ))
        .await;
    let feature_only = alice(&server)
        .get("repos/acme/public/activity?ref=refs/heads/feature&activity_type=force_push")
        .await;
    let mut unknown_filters = Vec::new();
    for filter in ["direction=up", "activity_type=merge", "time_period=decade"] {
        unknown_filters.push(
            alice(&server)
                .get(&format!("repos/acme/public/activity?per_page=100&{filter}"))
                .await,
        );
    }
    // Then
    let types = activity_types(&all);
    valid("repos/list-activities", &all);
    for kind in [
        "push",
        "force_push",
        "branch_deletion",
        "branch_creation",
        "pr_merge",
    ] {
        assert!(types.iter().any(|t| t == kind), "no {kind} in {types:?}");
    }
    assert_eq!(activity_types(&main_first), vec!["pr_merge"]);
    assert_eq!(
        valid("repos/list-activities", &main_first)[0]["after"],
        json!(merge.to_string())
    );
    assert_eq!(activity_types(&main_second), vec!["force_push"]);
    let forced = valid("repos/list-activities", &feature_only);
    assert_eq!(forced.as_array().unwrap().len(), 1);
    assert_eq!(forced[0]["before"], json!(feature.to_string()));
    assert_eq!(forced[0]["after"], json!(main.to_string()));
    assert_eq!(forced[0]["actor"]["login"], json!("alice"));
    for answer in &unknown_filters {
        assert_eq!(answer.status, StatusCode::OK);
        assert_eq!(activity_types(answer), types);
    }
}

#[tokio::test]
async fn codeowners_errors_name_each_bad_line_and_a_missing_file_is_not_found() {
    // Given
    let server = start(vec![json!({
        "kind": "repository", "owner": "acme", "name": "owned", "visibility": "public",
        "files": {".github/CODEOWNERS": "* @acme/nobody\ndocs/ @alice\n[x] @alice\n*.md @ghost\n"}
    })])
    .await;
    // When
    let owned = alice(&server)
        .get("repos/acme/owned/codeowners/errors")
        .await;
    let missing = alice(&server)
        .get("repos/acme/public/codeowners/errors")
        .await;
    // Then
    let body = valid("repos/codeowners-errors", &owned);
    let found: Vec<(i64, i64, String)> = body["errors"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            (
                e["line"].as_i64().unwrap(),
                e["column"].as_i64().unwrap(),
                e["kind"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    assert_eq!(
        found,
        vec![
            (1, 3, "Unknown owner".to_owned()),
            (3, 1, "Invalid pattern".to_owned()),
            (4, 6, "Unknown owner".to_owned()),
        ]
    );
    assert_eq!(body["errors"][0]["path"], json!(".github/CODEOWNERS"));
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn query_repository_answers_a_visible_alias_and_nulls_a_hidden_one() {
    // Given
    let server = start(vec![json!({
        "kind": "repository", "owner": "acme", "name": "secret", "visibility": "private"
    })])
    .await;
    let carol = Graphql::new(GoGithub::new(&server).with_token(CAROL_TOKEN));
    // When
    let answer = carol
        .query(
            "query { seen: repository(owner: \"acme\", name: \"private\") { id nameWithOwner } \
             hidden: repository(owner: \"acme\", name: \"secret\") { id } }",
            &json!({}),
        )
        .await;
    // Then
    assert_eq!(answer.status, StatusCode::OK);
    let body = answer.json();
    assert_eq!(body["data"]["seen"]["nameWithOwner"], json!("acme/private"));
    assert_eq!(
        body["data"]["seen"]["id"],
        json!(node_id::entity(node_id::REPOSITORY, 3002))
    );
    assert_eq!(body["data"]["hidden"], Value::Null);
    assert_eq!(body["errors"][0]["type"], json!("NOT_FOUND"));
    assert_eq!(body["errors"][0]["path"], json!(["hidden"]));
}

#[tokio::test]
async fn release_admin_writes_deliver_published_and_edited() {
    // Given
    let server = start(watcher().to_vec()).await;
    // When
    let created = server
        .admin(Method::POST, "release", Some(&release_body()))
        .await;
    let edited = server
        .admin(
            Method::PATCH,
            "releases/7001",
            Some(&json!({"name": "Two, renamed"})),
        )
        .await;
    let got = deliveries(&server, 4).await;
    // Then
    assert_eq!(created.status, StatusCode::CREATED);
    assert_eq!(edited.status, StatusCode::OK);
    assert_eq!(
        delivered_as(&got, "push", None)["ref"],
        json!("refs/tags/v2.0.0")
    );
    assert_eq!(delivered_as(&got, "create", None)["ref_type"], "tag");
    let published = delivered(&got, "release", "published");
    assert_eq!(published["release"]["tag_name"], json!("v2.0.0"));
    assert_eq!(published["repository"]["full_name"], json!("acme/public"));
    let edited = delivered(&got, "release", "edited");
    assert_eq!(edited["changes"]["name"]["from"], json!("Two"));
    assert_eq!(edited["release"]["name"], json!("Two, renamed"));
}

#[tokio::test]
async fn the_request_log_lists_the_event_and_its_delivery() {
    // Given
    let server = start(watcher().to_vec()).await;
    // When an admin write emits repository.created and its delivery is logged
    let create = server
        .admin(
            Method::POST,
            "repository",
            Some(&json!({"owner": "acme", "name": "fresh", "sender": "alice"})),
        )
        .await;
    deliveries(&server, 1).await;
    server.admin(Method::POST, "hooks/drain", None).await;
    let log = server.admin(Method::GET, "log", None).await.json();
    // Then the log has the event and the delivery after the request
    assert!(create.status.is_success(), "{:?}", create.body);
    let entries = log["entries"].as_array().unwrap();
    let event = entries
        .iter()
        .find(|e| e["kind"] == json!("event"))
        .unwrap_or_else(|| panic!("no event in {entries:?}"));
    assert_eq!(
        (&event["event"], &event["action"], &event["app_id"]),
        (&json!("repository"), &json!("created"), &json!(4002))
    );
    let delivery = entries
        .iter()
        .find(|e| e["kind"] == json!("delivery"))
        .unwrap_or_else(|| panic!("no delivery in {entries:?}"));
    assert_eq!(delivery["delivery_id"], event["delivery_id"]);
    assert_eq!(delivery["status_code"], json!(200));
    assert!(delivery["cursor"].as_u64() > event["cursor"].as_u64());
    // Then a wait for the delivery answers with it
    let waited = server
        .admin(
            Method::GET,
            "log/wait?match=delivery%20repository.created",
            None,
        )
        .await;
    assert_eq!(waited.status, StatusCode::OK);
    assert_eq!(waited.json()["cursor"], delivery["cursor"]);
}

#[tokio::test]
async fn a_transfer_from_an_organization_names_it_as_the_previous_owner() {
    // Given the watcher App is installed on the organization and on the new owner
    let mut seed = watcher().to_vec();
    seed.push(
        json!({"kind": "installation", "id": 5003, "app": "watcher", "account": "alice",
                     "repository_selection": "all"}),
    );
    let server = start(seed).await;
    // When acme transfers a repository to alice
    let transfer = server
        .admin(
            Method::PATCH,
            "repos/acme/public",
            Some(&json!({"owner": "alice", "sender": "alice"})),
        )
        .await;
    let got = deliveries(&server, 1).await;
    // Then the new owner's installation gets the event with acme as an organization
    assert_eq!(transfer.status, StatusCode::OK, "{:?}", transfer.body);
    let transferred = delivered(&got, "repository", "transferred");
    let from = &transferred["changes"]["owner"]["from"];
    assert_eq!(from["organization"]["login"], json!("acme"));
    assert_eq!(from.get("user"), None);
    assert_eq!(transferred["installation"]["id"], json!(5003));
}

#[tokio::test]
async fn repository_admin_writes_deliver_their_repository_actions() {
    // Given
    let server = start(watcher().to_vec()).await;
    // When
    let create = server
        .admin(
            Method::POST,
            "repository",
            Some(&json!({"owner": "acme", "name": "fresh", "sender": "alice"})),
        )
        .await;
    let edit = server
        .admin(
            Method::PATCH,
            "repos/acme/public",
            Some(
                &json!({"description": "prompts", "name": "prompts", "archived": true,
                         "private": true, "sender": "alice"}),
            ),
        )
        .await;
    let renamed = alice(&server).get("repos/acme/prompts").await;
    let public = server
        .admin(
            Method::PATCH,
            "repos/acme/prompts",
            Some(&json!({"private": false, "sender": "alice"})),
        )
        .await;
    assert_eq!(public.status, StatusCode::OK);
    let deleted = server
        .admin(Method::DELETE, "repos/acme/prompts", Some(&json!({})))
        .await;
    let gone = alice(&server).get("repos/acme/prompts").await;
    let got = deliveries(&server, 7).await;
    // Then
    assert!(create.status.is_success(), "{:?}", create.body);
    let created = delivered(&got, "repository", "created");
    assert_eq!(created["repository"]["full_name"], json!("acme/fresh"));
    assert_eq!(created["sender"]["login"], json!("alice"));
    assert_eq!(edit.status, StatusCode::OK);
    assert_eq!(edit.json()["full_name"], json!("acme/prompts"));
    assert_eq!(renamed.status, StatusCode::OK);
    assert_eq!(renamed.json()["archived"], json!(true));
    assert_eq!(renamed.json()["visibility"], json!("private"));
    assert_eq!(deleted.status, StatusCode::NO_CONTENT);
    assert_eq!(gone.status, StatusCode::NOT_FOUND);
    let edited = delivered(&got, "repository", "edited");
    assert_eq!(edited["changes"]["description"]["from"], Value::Null);
    assert_eq!(edited["sender"]["login"], json!("alice"));
    let renamed = delivered(&got, "repository", "renamed");
    assert_eq!(
        renamed["changes"]["repository"]["name"]["from"],
        json!("public")
    );
    assert_eq!(renamed["repository"]["name"], json!("prompts"));
    delivered(&got, "repository", "archived");
    delivered(&got, "repository", "privatized");
    assert_eq!(
        delivered(&got, "repository", "publicized")["repository"]["private"],
        false
    );
    delivered(&got, "repository", "deleted");
}
