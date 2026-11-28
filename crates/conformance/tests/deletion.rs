use http::header::{self, HeaderValue};
use http::{HeaderMap, Method};
use hubdouble_hooks::Hooks;
use hubdouble_store::{
    CheckRuns, Environments, IssueComments, Issues, Releases, Rulesets, Statuses, Teams,
};
use hubdouble_testkit::{Answer, GoGithub, Graphql, TestServer, fixture};
use serde_json::{Value, json};

const KEPT_REPO_ID: i64 = 3101;
const NOTES_REPO_ID: i64 = 3102;
const DOOMED_ORG_ID: i64 = 2101;
const DOOMED_REPO_ID: i64 = 3103;
const DOOMED_INSTALLATION_ID: i64 = 5101;
const TEMP_TEAM_ID: i64 = 6101;
const KEPT_TEAM_ID: i64 = 6102;
const HOOK_ADMIN_TOKEN: &str = "ghp_d0a1000000000000000000000000000000000";
const CAROL_APP_ID: i64 = 4002;
const CAROL_INSTALLATION_ID: i64 = 5102;
const CAROL_APP_JWT: &str = concat!(
    "eyJhbGciOiJSUzI1NiIsInR5cCI6IkpXVCJ9.eyJpc3MiOjQwMDIsImlhdCI6MTc2NzIyNTU0MCwiZXhwIjoxNzY3MjI2MjAwfQ.",
    "gobO4qnik78K0sFVCrmbD6FG02vW0XMALsekmCW_PF0t0GZsJUx0isvZDY84Zp54-2zB9ciCUTM48qfwPNVeTsO9B_8jFncvfp6",
    "f0uwi0SS8nQWyh7pZJs_7h_T2DlbIVHBjTTgXMtLvWpP5VIe9YjgYQp1TbepKztie1tg6DX61xly-vxhKH-nNZ6_D9zMFbgvEV",
    "GvNYzQl4KSX0BwtEHs-CFj_DunR-bAuDDaKKeKB2kC99uqfPNpWnlp03z1KYEiH3B7d9zj7nVMCE4RHf9NmXifV0Emikpkqsx2",
    "hWlO65sop7GoeCOQuKxK6QoYJjtCXQ14lIQyGZIpOOlEpDA"
);

const REPO_FEEDS: [&str; 8] = [
    "issues",
    "issues/comments",
    "pulls",
    "commits/main/check-runs",
    "commits/main/statuses",
    "releases",
    "rules/branches/main",
    "actions/variables",
];

fn repository(id: i64, owner: &str, name: &str) -> Value {
    json!({"kind": "repository", "id": id, "owner": owner, "name": name,
           "visibility": "public", "has_issues": true,
           "files": {"README.md": format!("# {name}\n")},
           "commits": [{"branch": "feature", "message": "add the feature",
                        "files": {"src/feature.txt": "feature\n"}}]})
}

fn repo_rows(owner: &str, name: &str) -> Vec<Value> {
    let full = format!("{owner}/{name}");
    vec![
        json!({"kind": "pull_request", "repository": full, "user": "alice",
               "title": "Ship the feature", "head": "feature", "base": "main"}),
        json!({"kind": "check_run", "repository": full, "app": fixture::APP_SLUG,
               "name": "build", "head_sha": "main"}),
        json!({"kind": "status", "repository": full, "sha": "main", "creator": "alice",
               "state": "pending", "context": "ci"}),
        json!({"kind": "release", "repository": full, "tag_name": "v2.0.0"}),
        json!({"kind": "ruleset", "repository": full, "name": "rules", "target": "branch",
               "enforcement": "active", "bypass_actors": [],
               "conditions": {"ref_name": {"include": ["~DEFAULT_BRANCH"], "exclude": []}},
               "rules": [{"type": "required_linear_history"}]}),
        json!({"kind": "actions_variable", "repository": full, "name": "STAGE",
               "value": "prod"}),
        json!({"kind": "environment", "repository": full, "name": "staging"}),
    ]
}

fn environments(server: &TestServer, repo_id: i64) -> Vec<String> {
    server
        .server
        .services()
        .store
        .view(|tx| {
            Ok(tx
                .table::<Environments>()
                .in_repo(repo_id)
                .map(|e| e.name.clone())
                .collect())
        })
        .unwrap_or_else(|error| panic!("{error}"))
}

fn carol_in_acme() -> Value {
    json!({"kind": "org_membership", "org": "acme", "login": "carol"})
}

async fn start(extra: Vec<Value>) -> TestServer {
    let mut builder = TestServer::builder()
        .frame(hubdouble_api::register_all)
        .fixture();
    for entry in extra {
        builder = builder.entry(entry);
    }
    builder.start().await
}

async fn admin(server: &TestServer, method: Method, path: &str, body: Option<&Value>) -> Answer {
    let answer = server.admin(method, path, body).await;
    assert!(
        answer.status.is_success(),
        "{path}: {} {:?}",
        answer.status,
        answer.body
    );
    answer
}

async fn status(client: &GoGithub, path: &str) -> u16 {
    client.get(path).await.status.as_u16()
}

async fn add_comment(client: &GoGithub, owner: &str, name: &str) {
    let answer = client
        .post(
            &format!("repos/{owner}/{name}/issues/1/comments"),
            &json!({"body": "looks good"}),
        )
        .await;
    assert_eq!(answer.status.as_u16(), 201, "{:?}", answer.body);
}

async fn add_hook(client: &GoGithub, path: &str) {
    let answer = client
        .post(
            path,
            &json!({"name": "web", "config": {"url": "https://hooks.example.com/"}}),
        )
        .await;
    assert_eq!(answer.status.as_u16(), 201, "{path}: {:?}", answer.body);
}

fn hook_count(server: &TestServer, owner_id: i64) -> usize {
    server
        .server
        .services()
        .store
        .view(|tx| Ok(tx.table::<Hooks>().children(owner_id).count()))
        .unwrap_or_else(|error| panic!("{error}"))
}

async fn feed_sizes(client: &GoGithub, owner: &str, name: &str) -> Vec<usize> {
    let mut sizes = Vec::new();
    for feed in REPO_FEEDS {
        let path = format!("repos/{owner}/{name}/{feed}");
        let answer = client.get(&path).await;
        assert_eq!(answer.status.as_u16(), 200, "{path}: {:?}", answer.body);
        let body = answer.json();
        let size = body.as_array().map_or_else(
            || {
                body["total_count"]
                    .as_u64()
                    .and_then(|n| usize::try_from(n).ok())
                    .unwrap_or_else(|| panic!("{path}: {body}"))
            },
            Vec::len,
        );
        sizes.push(size);
    }
    sizes
}

fn assert_no_repo_rows(server: &TestServer, repo_id: i64) {
    server
        .server
        .services()
        .store
        .view(|tx| {
            assert!(tx.table::<Issues>().in_repo(repo_id).next().is_none());
            assert!(
                tx.table::<IssueComments>()
                    .in_repo(repo_id)
                    .next()
                    .is_none()
            );
            assert!(tx.table::<CheckRuns>().in_repo(repo_id).next().is_none());
            assert!(tx.table::<Statuses>().in_repo(repo_id).next().is_none());
            assert!(tx.table::<Releases>().in_repo(repo_id).next().is_none());
            assert!(tx.table::<Rulesets>().in_repo(repo_id).next().is_none());
            Ok(())
        })
        .unwrap_or_else(|error| panic!("{error}"));
}

#[tokio::test]
async fn deleting_a_repository_drops_the_rows_of_every_feature_and_keeps_other_repositories() {
    // Given two repositories with an issue, a comment, a pull request, a check run, a status,
    // a release, a ruleset and an Actions variable each
    let mut extra = vec![repository(KEPT_REPO_ID, fixture::ORG, "kept")];
    extra.extend(repo_rows(fixture::ORG, fixture::PUBLIC_REPO));
    extra.extend(repo_rows(fixture::ORG, "kept"));
    let server = start(extra).await;
    let alice = GoGithub::new(&server).with_token(fixture::ALICE_TOKEN);
    for name in [fixture::PUBLIC_REPO, "kept"] {
        add_comment(&alice, fixture::ORG, name).await;
        add_hook(&alice, &format!("repos/{}/{name}/hooks", fixture::ORG)).await;
    }
    let kept = feed_sizes(&alice, fixture::ORG, "kept").await;
    assert!(kept.iter().all(|size| *size > 0), "{kept:?}");

    // When the public repository is deleted
    admin(&server, Method::DELETE, "repos/acme/public", None).await;

    // Then its reads answer 404, its rows are gone, and the other repository keeps its rows
    for feed in REPO_FEEDS {
        assert_eq!(
            status(&alice, &format!("repos/acme/public/{feed}")).await,
            404
        );
    }
    assert_no_repo_rows(&server, fixture::PUBLIC_REPO_ID);
    assert_eq!(hook_count(&server, fixture::PUBLIC_REPO_ID), 0);
    assert_eq!(
        environments(&server, fixture::PUBLIC_REPO_ID),
        Vec::<String>::new()
    );
    assert_eq!(feed_sizes(&alice, fixture::ORG, "kept").await, kept);
    assert_eq!(hook_count(&server, KEPT_REPO_ID), 1);
    assert_eq!(environments(&server, KEPT_REPO_ID), ["staging"]);

    // And a new repository with the same name starts empty
    let mut fresh = repository(0, fixture::ORG, fixture::PUBLIC_REPO);
    if let Some(entry) = fresh.as_object_mut() {
        entry.remove("id");
        entry.remove("kind");
    }
    admin(&server, Method::POST, "repository", Some(&fresh)).await;
    let fresh = feed_sizes(&alice, fixture::ORG, fixture::PUBLIC_REPO).await;
    assert!(fresh.iter().all(|size| *size == 0), "{fresh:?}");
}

#[tokio::test]
async fn deleting_an_environment_drops_it_and_keeps_the_other_environments() {
    // Given two environments of a repository
    let server = start(vec![
        json!({"kind": "environment", "repository": "acme/public", "name": "staging"}),
        json!({"kind": "environment", "repository": "acme/public", "name": "production"}),
    ])
    .await;

    // When one is deleted
    let path = "environments/acme/public/Staging";
    admin(&server, Method::DELETE, path, None).await;

    // Then it is gone, a second deletion answers 404, and the other environment stays
    assert_eq!(
        environments(&server, fixture::PUBLIC_REPO_ID),
        ["production"]
    );
    let again = server.admin(Method::DELETE, path, None).await;
    assert_eq!(again.status.as_u16(), 404);
}

#[tokio::test]
async fn deleting_a_team_drops_its_members_and_repositories_and_keeps_other_teams() {
    // Given a team with a member and a repository, and another team
    let server = start(vec![
        carol_in_acme(),
        json!({"kind": "team", "id": TEMP_TEAM_ID, "org": "acme", "name": "Temp",
               "members": [{"login": "carol"}],
               "repositories": [{"repository": "acme/public", "permission": "write"}]}),
        json!({"kind": "team", "id": KEPT_TEAM_ID, "org": "acme", "name": "Kept",
               "members": [{"login": "bob"}]}),
    ])
    .await;
    let alice = GoGithub::new(&server).with_token(fixture::ALICE_TOKEN);

    // When the first team is deleted
    admin(&server, Method::DELETE, "team/acme/temp", None).await;

    // Then it is gone from reads and the other team stays with its member
    assert_eq!(status(&alice, "orgs/acme/teams/temp").await, 404);
    assert_eq!(
        server
            .admin(Method::GET, "team/acme/temp", None)
            .await
            .status
            .as_u16(),
        404
    );
    let teams = alice.get("orgs/acme/teams").await.json();
    let slugs: Vec<&str> = teams
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|t| t["slug"].as_str())
        .collect();
    assert_eq!(slugs, ["kept"]);
    let kept = admin(&server, Method::GET, "team/acme/kept", None)
        .await
        .json();
    assert_eq!(kept["members"].as_array().map(Vec::len), Some(1));

    // And a new team with the same name has no members or repositories
    admin(
        &server,
        Method::POST,
        "team",
        Some(&json!({"org": "acme", "name": "Temp"})),
    )
    .await;
    let fresh = admin(&server, Method::GET, "team/acme/temp", None)
        .await
        .json();
    assert_eq!(fresh["members"], json!([]));
    assert_eq!(fresh["repos"], json!([]));
    server
        .server
        .services()
        .store
        .view(|tx| {
            assert!(tx.table::<Teams>().get(TEMP_TEAM_ID).is_none());
            Ok(())
        })
        .unwrap_or_else(|error| panic!("{error}"));
}

#[tokio::test]
async fn deleting_a_user_drops_their_repositories_memberships_and_tokens_and_keeps_other_users() {
    // Given carol with a repository and an issue in it, an org membership, a team membership
    // and a collaborator role
    let server = start(vec![
        repository(NOTES_REPO_ID, fixture::CAROL, "notes"),
        carol_in_acme(),
        json!({"kind": "team", "id": TEMP_TEAM_ID, "org": "acme", "name": "Temp",
               "members": [{"login": "carol"}, {"login": "bob"}]}),
    ])
    .await;
    let alice = GoGithub::new(&server).with_token(fixture::ALICE_TOKEN);
    let carol = GoGithub::new(&server).with_token(fixture::CAROL_TOKEN);
    admin(
        &server,
        Method::PUT,
        "repository/acme/public/collaborators/carol",
        Some(&json!({"permission": "write"})),
    )
    .await;
    let issue = carol
        .post("repos/carol/notes/issues", &json!({"title": "todo"}))
        .await;
    assert_eq!(issue.status.as_u16(), 201, "{:?}", issue.body);
    assert_eq!(status(&alice, "orgs/acme/members/carol").await, 204);

    // When carol is deleted
    admin(&server, Method::DELETE, "user/carol", None).await;

    // Then carol, her repository, her token and her memberships are gone
    assert_eq!(status(&alice, "users/carol").await, 404);
    assert_eq!(status(&alice, "repos/carol/notes").await, 404);
    assert_eq!(status(&carol, "user").await, 401);
    assert_eq!(status(&alice, "orgs/acme/members/carol").await, 404);
    assert_eq!(
        server
            .admin(Method::GET, "user/carol", None)
            .await
            .status
            .as_u16(),
        404
    );
    assert_no_repo_rows(&server, NOTES_REPO_ID);
    let team = admin(&server, Method::GET, "team/acme/temp", None)
        .await
        .json();
    assert_eq!(team["members"].as_array().map(Vec::len), Some(1));

    // And bob keeps his account and memberships
    assert_eq!(status(&alice, "users/bob").await, 200);
    assert_eq!(status(&alice, "orgs/acme/members/bob").await, 204);

    // And a new user with the same login has no memberships or roles
    admin(
        &server,
        Method::POST,
        "user",
        Some(&json!({"login": "carol", "tokens": [{"token": fixture::CAROL_TOKEN}]})),
    )
    .await;
    assert_eq!(status(&alice, "orgs/acme/members/carol").await, 404);
    let permission = alice
        .get("repos/acme/public/collaborators/carol/permission")
        .await
        .json();
    assert_ne!(permission["permission"], "write");
}

#[tokio::test]
async fn the_content_of_a_deleted_user_reads_back_as_the_ghost_user() {
    // Given an issue and a comment by carol in a repository she does not own
    let server = start(vec![carol_in_acme()]).await;
    let alice = GoGithub::new(&server).with_token(fixture::ALICE_TOKEN);
    let carol = GoGithub::new(&server).with_token(fixture::CAROL_TOKEN);
    let issue = carol
        .post("repos/acme/public/issues", &json!({"title": "by carol"}))
        .await;
    assert_eq!(issue.status.as_u16(), 201, "{:?}", issue.body);
    let number = issue.json()["number"].clone();
    let comment = carol
        .post(
            &format!("repos/acme/public/issues/{number}/comments"),
            &json!({"body": "from carol"}),
        )
        .await;
    assert_eq!(comment.status.as_u16(), 201, "{:?}", comment.body);
    let comment_id = comment.json()["id"].clone();
    let comment_node = comment.json()["node_id"].clone();

    // When carol is deleted
    admin(&server, Method::DELETE, "user/carol", None).await;

    // Then REST names the ghost user as the author of both
    let ghost = |user: &Value| {
        assert_eq!(user["login"], "ghost", "{user}");
        assert_eq!(user["id"], 10137, "{user}");
        assert_eq!(user["node_id"], "MDQ6VXNlcjEwMTM3", "{user}");
        assert_eq!(user["type"], "User", "{user}");
        assert!(
            user["html_url"]
                .as_str()
                .is_some_and(|u| u.ends_with("/ghost")),
            "{user}"
        );
    };
    for path in [
        format!("repos/acme/public/issues/{number}"),
        format!("repos/acme/public/issues/comments/{comment_id}"),
    ] {
        let answer = alice.get(&path).await;
        assert_eq!(answer.status.as_u16(), 200, "{path}: {:?}", answer.body);
        ghost(&answer.json()["user"]);
    }

    // And GraphQL names the ghost user as the author of both
    let answer = Graphql::new(alice)
        .query(
            "query($n: Int!, $c: ID!) { \
             repository(owner: \"acme\", name: \"public\") { \
             issue(number: $n) { author { login } } } \
             node(id: $c) { ... on IssueComment { author { login } } } }",
            &json!({"n": number, "c": comment_node}),
        )
        .await;
    let body = answer.json();
    assert_eq!(
        body["data"]["repository"]["issue"]["author"]["login"], "ghost",
        "{body}"
    );
    assert_eq!(body["data"]["node"]["author"]["login"], "ghost", "{body}");

    // And the ghost user's own profile answers as on GitHub, by login and by ID
    let alice = GoGithub::new(&server).with_token(fixture::ALICE_TOKEN);
    for path in ["users/ghost", "user/10137"] {
        let answer = alice.get(path).await;
        assert_eq!(answer.status.as_u16(), 200, "{path}: {:?}", answer.body);
        let user = answer.json();
        ghost(&user);
        assert_eq!(user["name"], "Deleted user", "{user}");
        assert_eq!(user["created_at"], "2008-05-13T06:14:25Z", "{user}");
    }
}

fn assert_ghost(path: &str, user: &Value) {
    assert_eq!(user["login"], "ghost", "{path}: {user}");
    assert_eq!(user["id"], 10137, "{path}: {user}");
}

#[tokio::test]
async fn releases_runs_statuses_activity_and_pushes_of_a_deleted_user_name_the_ghost_user() {
    // Given carol's push of a workflow, a release with an asset and a status in acme/public,
    // and a repository hook that takes pushes
    let server = start(vec![
        json!({"kind": "actions", "auto_complete": "none"}),
        json!({"kind": "repository", "owner": "acme", "name": "ci", "visibility": "public",
               "files": {".github/workflows/ci.yml": "on: workflow_dispatch\njobs: {test: {}}\n"},
               "collaborators": [{"login": "carol", "permission": "write"}],
               "commits": [{"branch": "feature", "message": "add the feature",
                            "files": {"src/feature.txt": "feature\n"}}]}),
        json!({"kind": "release", "repository": "acme/ci", "tag_name": "v9.0.0",
               "author": "carol", "assets": [{"name": "app.zip", "content": "zip"}]}),
    ])
    .await;
    let alice = GoGithub::new(&server).with_token(fixture::ALICE_TOKEN);
    let carol = GoGithub::new(&server).with_token(fixture::CAROL_TOKEN);
    let feature = alice
        .get("repos/acme/ci/git/ref/heads/feature")
        .await
        .json();
    let sha = feature["object"]["sha"]
        .as_str()
        .unwrap_or_else(|| panic!("{feature}"))
        .to_owned();
    let pushed = carol
        .call_json(
            Method::PATCH,
            "repos/acme/ci/git/refs/heads/main",
            &json!({"sha": sha}),
        )
        .await;
    assert_eq!(pushed.status.as_u16(), 200, "{:?}", pushed.body);
    let dispatched = carol
        .post(
            "repos/acme/ci/actions/workflows/ci.yml/dispatches",
            &json!({"ref": "main"}),
        )
        .await;
    assert!(dispatched.status.is_success(), "{:?}", dispatched.body);
    let status = carol
        .post(
            &format!("repos/acme/ci/statuses/{sha}"),
            &json!({"state": "success", "context": "ci"}),
        )
        .await;
    assert_eq!(status.status.as_u16(), 201, "{:?}", status.body);
    let hook = alice
        .post(
            "repos/acme/ci/hooks",
            &json!({"events": ["push"], "config": {"url": server.receiver.url(),
                    "content_type": "json"}}),
        )
        .await;
    assert_eq!(hook.status.as_u16(), 201, "{:?}", hook.body);
    let hook_id = hook.json()["id"].clone();

    // When carol is deleted
    admin(&server, Method::DELETE, "user/carol", None).await;

    // Then every read names the ghost user in carol's place
    let releases = alice.get("repos/acme/ci/releases").await;
    assert_eq!(releases.status.as_u16(), 200, "{:?}", releases.body);
    let release = alice.get("repos/acme/ci/releases/tags/v9.0.0").await;
    assert_eq!(release.status.as_u16(), 200, "{:?}", release.body);
    let release = release.json();
    assert_ghost("release author", &release["author"]);
    assert_ghost("asset uploader", &release["assets"][0]["uploader"]);
    let asset_id = &release["assets"][0]["id"];
    let asset = alice
        .get(&format!("repos/acme/ci/releases/assets/{asset_id}"))
        .await;
    assert_ghost("asset", &asset.json()["uploader"]);
    let statuses = alice
        .get(&format!("repos/acme/ci/commits/{sha}/statuses"))
        .await;
    assert_ghost("status creator", &statuses.json()[0]["creator"]);
    let runs = alice.get("repos/acme/ci/actions/runs").await.json();
    let run = &runs["workflow_runs"][0];
    assert_eq!(run["event"], "workflow_dispatch", "{runs}");
    assert_ghost("run actor", &run["actor"]);
    assert_ghost("run triggering actor", &run["triggering_actor"]);
    let activity = alice.get("repos/acme/ci/activity").await.json();
    assert_ghost("activity actor", &activity[0]["actor"]);

    // And the push test replays carol's push with the ghost user as its sender
    let drained = server.admin(Method::POST, "hooks/drain", None).await;
    assert_eq!(drained.status.as_u16(), 204);
    server.receiver.received().await;
    let tested = alice
        .call(
            Method::POST,
            &format!("repos/acme/ci/hooks/{hook_id}/tests"),
        )
        .await;
    assert_eq!(tested.status.as_u16(), 204, "{:?}", tested.body);
    let drained = server.admin(Method::POST, "hooks/drain", None).await;
    assert_eq!(drained.status.as_u16(), 204);
    let deliveries = server.receiver.received().await;
    let push = deliveries
        .iter()
        .find(|d| d.header("x-github-event") == "push")
        .unwrap_or_else(|| panic!("no push delivery"));
    assert_ghost("push sender", &push.json()["sender"]);
}

#[tokio::test]
async fn deleting_a_user_drops_the_apps_they_own_with_installations_and_tokens() {
    // Given carol's App installed on acme, with an installation token
    let server = start(vec![
        json!({"kind": "app", "id": CAROL_APP_ID, "slug": "carol-app", "owner": "carol",
               "client_id": "Iv1.carolapp", "device_flow": true,
               "public_key": fixture::APP_PUBLIC_KEY, "permissions": {"metadata": "read"}}),
        json!({"kind": "installation", "id": CAROL_INSTALLATION_ID, "app": "carol-app",
               "account": "acme", "permissions": {"metadata": "read"}}),
    ])
    .await;
    let app = GoGithub::enterprise(&server).with_token(CAROL_APP_JWT);
    assert_eq!(status(&app, "app").await, 200);
    let minted = app
        .post(
            &format!("app/installations/{CAROL_INSTALLATION_ID}/access_tokens"),
            &json!({}),
        )
        .await;
    assert_eq!(minted.status.as_u16(), 201, "{:?}", minted.body);
    let token = minted.json()["token"]
        .as_str()
        .unwrap_or_else(|| panic!("{:?}", minted.body))
        .to_owned();
    let installation = GoGithub::new(&server).with_token(&token);
    assert_eq!(
        status(&installation, "installation/repositories").await,
        200
    );
    let mut form = HeaderMap::new();
    form.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/x-www-form-urlencoded"),
    );
    form.insert(header::ACCEPT, HeaderValue::from_static("application/json"));
    let started = GoGithub::new(&server)
        .call_json_headers(
            Method::POST,
            "login/device/code",
            b"client_id=Iv1.carolapp".to_vec(),
            form,
        )
        .await;
    assert_eq!(started.status.as_u16(), 200, "{:?}", started.body);
    let user_code = started.json()["user_code"]
        .as_str()
        .unwrap_or_else(|| panic!("{:?}", started.body))
        .to_owned();

    // When carol is deleted
    admin(&server, Method::DELETE, "user/carol", None).await;

    // Then the App's JWT and its installation token are refused, and the installation is gone
    assert_eq!(status(&app, "app").await, 401);
    assert_eq!(
        status(&installation, "installation/repositories").await,
        401
    );
    for path in [
        format!("installation/{CAROL_INSTALLATION_ID}"),
        "app/carol-app".to_owned(),
    ] {
        let answer = server.admin(Method::GET, &path, None).await;
        assert_eq!(answer.status.as_u16(), 404, "{path}");
    }

    // And its bot account and its pending device code are gone, so a new App takes the slug
    let alice = GoGithub::new(&server).with_token(fixture::ALICE_TOKEN);
    assert_eq!(status(&alice, "users/carol-app[bot]").await, 404);
    let decided = server
        .admin(
            Method::POST,
            &format!("oauth/device/{user_code}"),
            Some(&json!({"login": fixture::BOB})),
        )
        .await;
    assert_eq!(decided.status.as_u16(), 404);
    admin(
        &server,
        Method::POST,
        "app",
        Some(&json!({"slug": "carol-app", "owner": "bob",
                     "public_key": fixture::APP_PUBLIC_KEY})),
    )
    .await;

    // And the App of another owner stays
    admin(&server, Method::GET, "app/fixture-app", None).await;
}

#[tokio::test]
async fn an_account_cannot_be_renamed_to_the_ghost_login() {
    // Given carol
    let server = start(Vec::new()).await;

    // When carol is renamed to the ghost user's login, in any case
    for login in ["ghost", "Ghost"] {
        let renamed = server
            .admin(
                Method::PATCH,
                "installation-target/carol",
                Some(&json!({"login": login})),
            )
            .await;

        // Then the rename is refused and carol keeps her login
        assert_eq!(renamed.status.as_u16(), 422, "{login}");
    }
    let alice = GoGithub::new(&server).with_token(fixture::ALICE_TOKEN);
    assert_eq!(status(&alice, "users/carol").await, 200);
}

#[tokio::test]
async fn deleting_a_parent_team_deletes_its_child_teams_and_keeps_other_teams() {
    // Given a team with a child and a grandchild, and another team
    let server = start(vec![
        json!({"kind": "team", "org": "acme", "name": "Root"}),
        json!({"kind": "team", "org": "acme", "name": "Child", "parent": "root"}),
        json!({"kind": "team", "org": "acme", "name": "Grandchild", "parent": "child"}),
        json!({"kind": "team", "org": "acme", "name": "Other"}),
    ])
    .await;
    let alice = GoGithub::new(&server).with_token(fixture::ALICE_TOKEN);

    // When the root team is deleted
    admin(&server, Method::DELETE, "team/acme/root", None).await;

    // Then its child and grandchild are gone with it, and the other team stays
    for slug in ["root", "child", "grandchild"] {
        assert_eq!(
            status(&alice, &format!("orgs/acme/teams/{slug}")).await,
            404,
            "{slug}"
        );
    }
    assert_eq!(status(&alice, "orgs/acme/teams/other").await, 200);
}

#[tokio::test]
async fn a_deleted_user_or_team_leaves_no_assignment_or_review_request() {
    // Given an issue assigned to carol, and a pull request that asks carol and a team to review
    let server = start(vec![
        carol_in_acme(),
        json!({"kind": "team", "id": TEMP_TEAM_ID, "org": "acme", "name": "Temp",
               "members": [{"login": "bob"}],
               "repositories": [{"repository": "acme/public", "permission": "write"}]}),
        json!({"kind": "pull_request", "repository": "acme/public", "user": "alice",
               "title": "Ship the feature", "head": "feature", "base": "main"}),
    ])
    .await;
    admin(
        &server,
        Method::PUT,
        "repository/acme/public/collaborators/carol",
        Some(&json!({"permission": "write"})),
    )
    .await;
    let alice = GoGithub::new(&server).with_token(fixture::ALICE_TOKEN);
    let issue = alice
        .post(
            "repos/acme/public/issues",
            &json!({"title": "for carol", "assignees": ["carol"]}),
        )
        .await;
    assert_eq!(issue.status.as_u16(), 201, "{:?}", issue.body);
    let number = issue.json()["number"].clone();
    let requested = alice
        .post(
            "repos/acme/public/pulls/1/requested_reviewers",
            &json!({"reviewers": ["carol"], "team_reviewers": ["temp"]}),
        )
        .await;
    assert_eq!(requested.status.as_u16(), 201, "{:?}", requested.body);

    // When carol and the team are deleted
    admin(&server, Method::DELETE, "user/carol", None).await;
    admin(&server, Method::DELETE, "team/acme/temp", None).await;

    // Then the issue counts as unassigned and the pull request asks no one to review
    let unassigned = alice
        .get("repos/acme/public/issues?assignee=none")
        .await
        .json();
    let numbers: Vec<&Value> = unassigned
        .as_array()
        .into_iter()
        .flatten()
        .map(|i| &i["number"])
        .collect();
    assert!(numbers.contains(&&number), "{unassigned}");
    let reviewers = alice
        .get("repos/acme/public/pulls/1/requested_reviewers?per_page=1")
        .await;
    assert_eq!(reviewers.json(), json!({"users": [], "teams": []}));
    assert!(
        reviewers.header("link").is_empty(),
        "{}",
        reviewers.header("link")
    );
}

#[tokio::test]
async fn deleting_an_app_drops_its_bot_account_and_frees_its_slug() {
    // Given an App without installations
    let spare = json!({"slug": "spare-app", "owner": "acme",
                       "public_key": fixture::APP_PUBLIC_KEY});
    let mut seeded = spare.clone();
    seeded["kind"] = json!("app");
    let server = start(vec![seeded]).await;
    let alice = GoGithub::new(&server).with_token(fixture::ALICE_TOKEN);
    assert_eq!(status(&alice, "users/spare-app[bot]").await, 200);

    // When the App is deleted
    admin(&server, Method::DELETE, "app/spare-app", None).await;

    // Then its bot account is gone and a new App takes the slug
    assert_eq!(status(&alice, "users/spare-app[bot]").await, 404);
    admin(&server, Method::POST, "app", Some(&spare)).await;
}

#[tokio::test]
async fn deleting_an_org_drops_its_repositories_teams_and_installations_and_keeps_other_orgs() {
    // Given an org with a member, a repository with an issue, a team and an App installation
    let server = start(vec![
        json!({"kind": "user", "login": "dana",
               "tokens": [{"token": HOOK_ADMIN_TOKEN, "scopes": ["admin:org_hook"]}]}),
        json!({"kind": "org", "id": DOOMED_ORG_ID, "login": "doomed",
               "members": [{"login": "alice", "role": "admin"}, {"login": "bob"},
                           {"login": "dana", "role": "admin"}]}),
        repository(DOOMED_REPO_ID, "doomed", "app"),
        json!({"kind": "team", "org": "doomed", "name": "Core", "members": [{"login": "bob"}]}),
        json!({"kind": "installation", "id": DOOMED_INSTALLATION_ID, "app": fixture::APP_SLUG,
               "account": "doomed", "repositories": ["app"],
               "permissions": {"issues": "write", "metadata": "read"}}),
    ])
    .await;
    let alice = GoGithub::new(&server).with_token(fixture::ALICE_TOKEN);
    let issue = alice
        .post("repos/doomed/app/issues", &json!({"title": "todo"}))
        .await;
    assert_eq!(issue.status.as_u16(), 201, "{:?}", issue.body);
    let dana = GoGithub::new(&server).with_token(HOOK_ADMIN_TOKEN);
    add_hook(&dana, "orgs/doomed/hooks").await;
    assert_eq!(hook_count(&server, DOOMED_ORG_ID), 1);

    // When the org is deleted
    admin(&server, Method::DELETE, "org/doomed", None).await;

    // Then the org, its repository, team, memberships and installation are gone
    assert_eq!(status(&alice, "orgs/doomed/members/alice").await, 404);
    assert_eq!(status(&alice, "repos/doomed/app").await, 404);
    assert_eq!(status(&alice, "orgs/doomed/teams/core").await, 404);
    assert_eq!(
        server
            .admin(
                Method::GET,
                &format!("installation/{DOOMED_INSTALLATION_ID}"),
                None
            )
            .await
            .status
            .as_u16(),
        404
    );
    assert_no_repo_rows(&server, DOOMED_REPO_ID);
    assert_eq!(hook_count(&server, DOOMED_ORG_ID), 0);

    // And the other org, its repository and the App stay
    assert_eq!(status(&alice, "orgs/acme/members/bob").await, 204);
    assert_eq!(status(&alice, "repos/acme/public").await, 200);
    admin(&server, Method::GET, "app/fixture-app", None).await;

    // And a new org with the same login has no members or teams
    admin(
        &server,
        Method::POST,
        "org",
        Some(&json!({"login": "doomed"})),
    )
    .await;
    for path in ["org/doomed/members/bob", "team/doomed/core"] {
        let answer = server.admin(Method::GET, path, None).await;
        assert_eq!(answer.status.as_u16(), 404, "{path}");
    }
}
