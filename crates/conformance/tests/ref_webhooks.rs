use std::sync::Arc;

use http::{Method, StatusCode};
use hubdouble_admin::Registrar;
use hubdouble_auth::Actor;
use hubdouble_git::{
    Commit, EntryMode, Git, ObjectData, ObjectId, RefChange, Signature, TreeEntry,
};
use hubdouble_gql::{Args, Ctx, ErrorType, GqlError, MutCtx, Resolved, Target};
use hubdouble_model::{
    Access, ApiError, Delivery, Grant, Repo, Requirement, Scope, Timestamp, node_id,
};
use hubdouble_store::{Deliveries, Table, TxMut};
use hubdouble_testkit::fixture::{
    ALICE_ID, ALICE_TOKEN, APP_ID, APP_PUBLIC_KEY, APP_SLUG, BOB_TOKEN, INSTALLATION_ID,
    PUBLIC_REPO_ID, WEBHOOK_SECRET, WEBHOOK_URL_ENV,
};
use hubdouble_testkit::{GoGithub, Graphql, TestServer, standins};
use serde_json::{Value, json};

const MAIN: &str = "refs/heads/main";

#[tokio::test]
async fn creating_a_branch_delivers_a_signed_create_webhook() {
    // Given
    let server = TestServer::builder()
        .frame(hubdouble_api::register_all)
        .fixture()
        .start()
        .await;
    let client = GoGithub::new(&server).with_token(ALICE_TOKEN);
    let refs = "repos/acme/public/git";
    let main = client.get(&format!("{refs}/ref/heads/main")).await;
    assert_eq!(main.status, StatusCode::OK);
    let sha = main.json()["object"]["sha"].clone();

    // When
    let created = client
        .post(
            &format!("{refs}/refs"),
            &json!({"ref": "refs/heads/new-branch", "sha": sha}),
        )
        .await;
    assert_eq!(created.status, StatusCode::CREATED);
    let drained = server.admin(Method::POST, "hooks/drain", None).await;
    assert_eq!(drained.status, StatusCode::NO_CONTENT);

    // Then
    let received = server.receiver.received().await;
    let mut create = received
        .iter()
        .filter(|d| d.header("x-github-event") == "create");
    let delivery = create
        .next()
        .unwrap_or_else(|| panic!("expected a create delivery"));
    assert!(create.next().is_none());
    assert!(delivery.signed_with(WEBHOOK_SECRET));
    let payload = delivery.json();
    assert_eq!(payload["ref"], "new-branch");
    assert_eq!(payload["ref_type"], "branch");
    assert_eq!(payload["master_branch"], "main");
    assert_eq!(payload["installation"]["id"], INSTALLATION_ID);
    assert_eq!(payload["repository"]["id"], PUBLIC_REPO_ID);
    assert_eq!(payload["sender"]["id"], ALICE_ID);
}

#[tokio::test]
async fn deleting_a_branch_or_a_tag_delivers_a_signed_delete_webhook() {
    // Given
    let server = TestServer::builder()
        .frame(hubdouble_api::register_all)
        .fixture()
        .entry(
            json!({"kind": "app", "id": 4100, "slug": "deletions", "owner": "acme",
               "public_key": APP_PUBLIC_KEY,
               "webhook": {"url": format!("{{{{env \"{WEBHOOK_URL_ENV}\"}}}}"),
                           "secret": WEBHOOK_SECRET},
               "permissions": {"contents": "read", "metadata": "read"},
               "events": ["delete"]}),
        )
        .entry(
            json!({"kind": "installation", "id": 5100, "app": "deletions",
               "account": "acme", "repository_selection": "all"}),
        )
        .start()
        .await;
    let client = GoGithub::new(&server).with_token(ALICE_TOKEN);
    let refs = "repos/acme/public/git";
    let sha = client.get(&format!("{refs}/ref/heads/main")).await.json()["object"]["sha"].clone();
    for name in ["refs/heads/gone", "refs/tags/v9"] {
        let created = client
            .post(&format!("{refs}/refs"), &json!({"ref": name, "sha": sha}))
            .await;
        assert_eq!(created.status, StatusCode::CREATED);
    }

    // When
    for name in ["heads/gone", "tags/v9"] {
        let deleted = client
            .call(Method::DELETE, &format!("{refs}/refs/{name}"))
            .await;
        assert_eq!(deleted.status, StatusCode::NO_CONTENT);
    }
    let drained = server.admin(Method::POST, "hooks/drain", None).await;
    assert_eq!(drained.status, StatusCode::NO_CONTENT);

    // Then
    let received = server.receiver.received().await;
    let deletes: Vec<_> = received
        .iter()
        .filter(|d| d.header("x-github-event") == "delete")
        .collect();
    let got: Vec<_> = deletes
        .iter()
        .map(|d| {
            assert!(d.signed_with(WEBHOOK_SECRET));
            let payload = d.json();
            assert_eq!(payload["pusher_type"], "user");
            assert_eq!(payload["installation"]["id"], 5100);
            assert_eq!(payload["repository"]["id"], PUBLIC_REPO_ID);
            assert_eq!(payload["organization"]["login"], "acme");
            assert_eq!(payload["sender"]["id"], ALICE_ID);
            (payload["ref"].clone(), payload["ref_type"].clone())
        })
        .collect();
    assert_eq!(
        got,
        [
            (json!("gone"), json!("branch")),
            (json!("v9"), json!("tag"))
        ]
    );
}

#[tokio::test]
async fn an_injected_event_is_signed_and_logged_for_the_app_it_names() {
    // Given
    let server = TestServer::builder()
        .frame(hubdouble_api::register_all)
        .fixture()
        .start()
        .await;
    let delete = json!({"event": "delete", "app": APP_SLUG, "installation": INSTALLATION_ID,
        "repository": "acme/public", "sender": "alice",
        "payload": {"ref": "gone", "ref_type": "branch", "pusher_type": "user"}});
    let revoked = json!({"event": "github_app_authorization", "action": "revoked",
        "hook": APP_ID, "sender": "alice"});
    let stray = json!({"event": "delete", "app": APP_SLUG, "installation": APP_ID,
        "payload": {"ref": "gone", "ref_type": "branch", "pusher_type": "user"}});

    // When
    let mut statuses = Vec::new();
    for body in [&delete, &revoked, &stray] {
        statuses.push(
            server
                .admin(Method::POST, "events", Some(body))
                .await
                .status,
        );
    }
    let drained = server.admin(Method::POST, "hooks/drain", None).await;
    assert_eq!(drained.status, StatusCode::NO_CONTENT);

    // Then
    assert_eq!(
        statuses,
        [
            StatusCode::NO_CONTENT,
            StatusCode::NO_CONTENT,
            StatusCode::UNPROCESSABLE_ENTITY
        ]
    );
    let received = server.receiver.received().await;
    let [deleted, revoked] = &received[..] else {
        panic!("expected two deliveries, got {}", received.len());
    };
    assert!(deleted.signed_with(WEBHOOK_SECRET) && revoked.signed_with(WEBHOOK_SECRET));
    assert_eq!(deleted.header("x-github-event"), "delete");
    let payload = deleted.json();
    assert_eq!(payload["ref"], "gone");
    assert_eq!(payload["installation"]["id"], INSTALLATION_ID);
    assert_eq!(payload["repository"]["id"], PUBLIC_REPO_ID);
    assert_eq!(payload["organization"]["login"], "acme");
    assert_eq!(payload["sender"]["id"], ALICE_ID);
    assert_eq!(revoked.header("x-github-event"), "github_app_authorization");
    assert_eq!(revoked.json()["action"], "revoked");
    assert_eq!(revoked.json()["sender"]["id"], ALICE_ID);
    let logged: Vec<_> = deliveries(&server)
        .iter()
        .map(|d| {
            (
                d.event.clone(),
                d.action.clone(),
                d.installation_id,
                d.repo_id,
                d.guid.clone(),
            )
        })
        .collect();
    assert_eq!(
        logged,
        [
            (
                "delete".to_owned(),
                None,
                Some(INSTALLATION_ID),
                Some(PUBLIC_REPO_ID),
                deleted.header("x-github-delivery").to_owned()
            ),
            (
                "github_app_authorization".to_owned(),
                Some("revoked".to_owned()),
                None,
                None,
                revoked.header("x-github-delivery").to_owned()
            ),
        ]
    );
}

#[tokio::test]
async fn an_injected_push_has_the_push_form_of_the_repository() {
    // Given
    let server = TestServer::builder()
        .frame(hubdouble_api::register_all)
        .fixture()
        .start()
        .await;
    let sha = "0".repeat(40);
    let push = json!({"event": "push", "app": APP_SLUG, "installation": INSTALLATION_ID,
        "repository": "acme/public", "sender": "alice",
        "payload": {"ref": "refs/heads/main", "before": sha, "after": sha, "created": false,
                    "deleted": false, "forced": false, "base_ref": null,
                    "compare": "https://github.com/acme/public/compare/main",
                    "commits": [], "head_commit": null,
                    "pusher": {"name": "alice", "email": "alice@example.com"}}});

    // When
    let injected = server.admin(Method::POST, "events", Some(&push)).await;
    let drained = server.admin(Method::POST, "hooks/drain", None).await;

    // Then
    assert_eq!(injected.status, StatusCode::NO_CONTENT);
    assert_eq!(drained.status, StatusCode::NO_CONTENT);
    let received = server.receiver.received().await;
    let [delivered] = &received[..] else {
        panic!("expected one delivery, got {}", received.len());
    };
    let repository = &delivered.json()["repository"];
    assert_eq!(repository["url"], repository["html_url"]);
    assert_eq!(repository["master_branch"], "main");
    assert_eq!(repository["organization"], "acme");
    assert!(repository["created_at"].is_i64(), "{repository}");
}

#[derive(Clone, Default)]
struct Moves(Vec<RefChange>);

impl Table for Moves {}

#[expect(
    clippy::unnecessary_wraps,
    reason = "a ref subscriber returns a Result"
)]
fn record_move(tx: &mut TxMut<'_>, _: &Git, change: &RefChange) -> Result<(), ApiError> {
    tx.table_mut::<Moves>().get_mut().0.push(change.clone());
    Ok(())
}

fn repository_node(ctx: &Ctx<'_>, id: i64) -> Result<Resolved, GqlError> {
    ctx.load(Target::RepoId(id))
}

const CONTENTS_WRITE: Requirement = Requirement {
    any_of: &[Grant {
        permission: "contents",
        access: Access::Write,
    }],
    all_of: &[],
    scope: Scope::Repo,
    scopes: &["repo"],
};

fn unprocessable(message: &str) -> GqlError {
    GqlError::new(ErrorType::Unprocessable, message)
}

fn oid(v: &Value) -> Result<Option<ObjectId>, GqlError> {
    v.as_str()
        .map(|s| s.parse().map_err(|_| unprocessable("bad object ID")))
        .transpose()
}

fn update_refs(m: &mut MutCtx<'_>, args: &Args) -> Result<Resolved, GqlError> {
    let input: Value = args
        .get("input")?
        .ok_or_else(|| unprocessable("input is required"))?;
    let id = input["repositoryId"].as_str().unwrap_or_default();
    let Resolved::Object { value, .. } = m.load(Target::Node(id))? else {
        return Err(GqlError::new(ErrorType::NotFound, "no repository"));
    };
    let repo = value
        .downcast::<Repo>()
        .map_err(|_| GqlError::new(ErrorType::Internal, "not a repository"))?;
    m.authorize(&CONTENTS_WRITE, &repo)?;
    let sender = match m.actor {
        Actor::User { user, .. } => user.id,
        Actor::Installation { bot, .. } => bot.id,
        Actor::Anonymous | Actor::App { .. } => {
            return Err(GqlError::new(ErrorType::Forbidden, "no sender"));
        }
    };
    for update in input["refUpdates"].as_array().into_iter().flatten() {
        let name = update["name"].as_str().unwrap_or_default();
        let force = update["force"].as_bool().unwrap_or(false);
        m.git
            .update_ref(
                &mut m.tx,
                repo.id,
                sender,
                name,
                oid(&update["beforeOid"])?,
                oid(&update["afterOid"])?,
                force,
            )
            .map_err(|e| unprocessable(&e.to_string()))?;
    }
    Ok(Resolved::Null)
}

fn exit_frame(r: &mut Registrar<'_>) {
    r.store.register_table::<Moves>();
    r.git.on_ref_update(record_move);
    r.gql.object::<Repo>("Repository");
    r.gql
        .node(node_id::REPOSITORY, "Repository", repository_node);
    r.gql.mutation("updateRefs", update_refs);
}

async fn start() -> TestServer {
    TestServer::builder()
        .fixture()
        .register(standins::query_repository)
        .register(exit_frame)
        .start()
        .await
}

fn must<T>(r: Result<T, ApiError>, what: &str) -> T {
    r.unwrap_or_else(|e| panic!("{what}: {e:?}"))
}

fn moves(server: &TestServer) -> Vec<RefChange> {
    let store = &server.server.services().store;
    must(
        store.view(|tx| Ok(tx.table::<Moves>().0.clone())),
        "read the moves",
    )
}

fn deliveries(server: &TestServer) -> Vec<Arc<Delivery>> {
    let store = &server.server.services().store;
    must(
        store.view(|tx| {
            Ok(tx
                .table::<Deliveries>()
                .iter()
                .filter(|d| d.app_id == APP_ID)
                .cloned()
                .collect())
        }),
        "read the delivery log",
    )
}

fn resolve(server: &TestServer, name: &str) -> ObjectId {
    let services = server.server.services();
    must(
        services
            .store
            .view(|tx| Ok(services.git.resolve_ref(tx, PUBLIC_REPO_ID, name)?)),
        name,
    )
}

fn push_a_commit_to_main(server: &TestServer) -> (ObjectId, ObjectId) {
    let services = server.server.services();
    let git = &services.git;
    let pushed = services.store.update(|tx| {
        let old = git.resolve_ref(tx, PUBLIC_REPO_ID, MAIN)?;
        let ObjectData::Commit(parent) = git.object(tx, PUBLIC_REPO_ID, old)?.data else {
            return Err(ApiError::internal("main is not a commit"));
        };
        let ObjectData::Tree(entries) = git.object(tx, PUBLIC_REPO_ID, parent.tree)?.data else {
            return Err(ApiError::internal("the commit's tree is not a tree"));
        };
        let blob = git.write_blob(tx, PUBLIC_REPO_ID, b"exit\n")?;
        let mut entries = entries.to_vec();
        entries.push(TreeEntry {
            name: "EXIT.md".to_owned(),
            mode: EntryMode::File,
            id: blob,
        });
        let tree = git.write_tree(tx, PUBLIC_REPO_ID, &entries)?;
        let alice = Signature {
            name: "Alice".to_owned(),
            email: "alice@example.com".to_owned(),
            time: Timestamp::SEED,
            offset_minutes: 0,
        };
        let commit = Commit {
            tree,
            parents: vec![old],
            author: alice.clone(),
            committer: alice,
            message: "add the exit note\n".to_owned(),
            gpg_signature: None,
        };
        let new = git.write_commit(tx, PUBLIC_REPO_ID, &commit)?;
        git.update_ref(
            tx,
            PUBLIC_REPO_ID,
            ALICE_ID,
            MAIN,
            Some(old),
            Some(new),
            false,
        )?;
        Ok((old, new))
    });
    must(pushed, "push to main")
}

fn is_lowercase_uuid(s: &str) -> bool {
    let groups: Vec<&str> = s.split('-').collect();
    groups.iter().map(|g| g.len()).eq([8, 4, 4, 4, 12])
        && groups
            .iter()
            .all(|g| g.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')))
}

#[tokio::test]
async fn a_branch_push_reaches_the_subscriber_and_a_signed_logged_delivery_before_drain_returns() {
    // Given
    let server = start().await;
    let seeded = moves(&server).len();
    // When
    let (old, new) = push_a_commit_to_main(&server);
    let drained = server.admin(Method::POST, "hooks/drain", None).await;
    let received = server.receiver.received().await;
    let logged = deliveries(&server);
    // Then
    assert_eq!(drained.status, StatusCode::NO_CONTENT);
    assert_eq!(
        moves(&server)[seeded..],
        [RefChange {
            repo_id: PUBLIC_REPO_ID,
            sender: ALICE_ID,
            name: MAIN.to_owned(),
            old: Some(old),
            new: Some(new),
            forced: false,
        }]
    );
    let [delivery] = &received[..] else {
        panic!("expected one delivery, got {}", received.len());
    };
    let [entry] = &logged[..] else {
        panic!("expected one logged delivery, got {}", logged.len());
    };

    assert_eq!(delivery.path, "/webhook");
    assert!(delivery.signed_with(WEBHOOK_SECRET));
    assert!(!delivery.signed_with("another-secret"));
    assert!(delivery.header("x-hub-signature").starts_with("sha1="));
    assert_eq!(delivery.header("x-github-event"), "push");
    assert!(is_lowercase_uuid(delivery.header("x-github-delivery")));
    assert_eq!(delivery.header("x-github-delivery"), entry.guid);
    assert_eq!(
        delivery.header("x-github-hook-id"),
        entry.hook_id.to_string()
    );
    assert_eq!(
        delivery.header("x-github-hook-installation-target-type"),
        "integration"
    );
    assert_eq!(
        delivery.header("x-github-hook-installation-target-id"),
        APP_ID.to_string()
    );
    assert_eq!(delivery.header("content-type"), "application/json");
    assert_eq!(delivery.header("user-agent"), "GitHub-Hookshot/hubdouble");

    let payload = delivery.json();
    assert_eq!(payload["ref"], json!(MAIN));
    assert_eq!(payload["before"], json!(old.to_string()));
    assert_eq!(payload["after"], json!(new.to_string()));
    assert_eq!(payload["created"], json!(false));
    assert_eq!(payload["deleted"], json!(false));
    assert_eq!(payload["forced"], json!(false));
    assert_eq!(payload["commits"][0]["id"], json!(new.to_string()));
    assert_eq!(payload["commits"][0]["added"], json!(["EXIT.md"]));
    assert_eq!(payload["commits"][0]["removed"], json!([]));
    assert_eq!(payload["commits"][0]["modified"], json!([]));
    assert_eq!(payload["head_commit"]["id"], json!(new.to_string()));
    assert_eq!(payload["installation"]["id"], json!(INSTALLATION_ID));
    assert_eq!(payload["repository"]["id"], json!(PUBLIC_REPO_ID));
    assert_eq!(payload["repository"]["full_name"], json!("acme/public"));
    assert_eq!(payload["sender"]["id"], json!(ALICE_ID));

    assert_eq!(entry.event, "push");
    assert_eq!(entry.action, None);
    assert_eq!(entry.installation_id, Some(INSTALLATION_ID));
    assert_eq!(entry.repo_id, Some(PUBLIC_REPO_ID));
    assert!(!entry.redelivery);
    assert_eq!(entry.status_code, 200);
    assert_eq!(
        serde_json::from_slice::<Value>(&entry.request.payload).unwrap(),
        payload
    );
}

#[tokio::test]
async fn graphql_hides_one_alias_and_a_denied_mutation_leaves_the_ref_and_sends_nothing() {
    // Given
    let server = start().await;
    let bob = Graphql::new(GoGithub::new(&server).with_token(BOB_TOKEN));
    let alice = Graphql::new(GoGithub::new(&server).with_token(ALICE_TOKEN));
    let aliases = "query { visible: repository(owner: \"acme\", name: \"public\") { name } \
                   hidden: repository(owner: \"acme\", name: \"private\") { name } }";
    let main = resolve(&server, MAIN);
    let feature = resolve(&server, "refs/heads/feature");
    let seeded = moves(&server);
    let mutation = "mutation($id: ID!, $before: GitObjectID!, $after: GitObjectID!) { \
                    updateRefs(input: {repositoryId: $id, refUpdates: [\
                    {name: \"refs/heads/main\", beforeOid: $before, afterOid: $after}]}) \
                    { clientMutationId } }";
    let variables = json!({
        "id": node_id::entity(node_id::REPOSITORY, PUBLIC_REPO_ID),
        "before": main.to_string(),
        "after": feature.to_string(),
    });
    // When
    let as_bob = bob.query(aliases, &json!({})).await;
    let as_alice = alice.query(aliases, &json!({})).await;
    let denied = bob.query(mutation, &variables).await;
    let drained = server.admin(Method::POST, "hooks/drain", None).await;
    // Then
    assert_eq!(as_bob.status, StatusCode::OK);
    let as_bob = as_bob.json();
    assert_eq!(as_bob["data"]["visible"], json!({"name": "public"}));
    assert_eq!(as_bob["data"]["hidden"], Value::Null);
    let errors = as_bob["errors"].as_array().unwrap();
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0]["type"], json!("NOT_FOUND"));
    assert_eq!(errors[0]["path"], json!(["hidden"]));

    let as_alice = as_alice.json();
    assert_eq!(
        as_alice["data"],
        json!({"visible": {"name": "public"}, "hidden": {"name": "private"}})
    );
    assert_eq!(as_alice.get("errors"), None);

    assert_eq!(denied.status, StatusCode::OK);
    let denied = denied.json();
    assert_eq!(denied["data"]["updateRefs"], Value::Null);
    assert_eq!(denied["errors"][0]["type"], json!("FORBIDDEN"));
    assert_eq!(denied["errors"][0]["path"], json!(["updateRefs"]));
    assert_eq!(resolve(&server, MAIN), main);
    assert_eq!(moves(&server), seeded);
    assert_eq!(drained.status, StatusCode::NO_CONTENT);
    assert!(server.receiver.received().await.is_empty());
    assert_eq!(deliveries(&server), Vec::<Arc<Delivery>>::new());
}
