#![expect(
    clippy::unwrap_used,
    reason = "a test helper fails loudly on a broken fixture"
)]

use std::collections::BTreeMap;

use http::{Method, StatusCode};
use hubdouble_git::{EntryMode, FileChange, ObjectData, ObjectId, ObjectKind, Signature, Tag};
use hubdouble_model::{
    Access, Clock, Enforcement, Ruleset, RulesetSource, RulesetTarget, Timestamp, Token, TokenKind,
    node_id,
};
use hubdouble_store::{Installations, Repos, Rulesets, Tokens};
use hubdouble_testkit::fixture::{
    ALICE_ID, ALICE_TOKEN, APP_ID, BOB, BOB_ID, BOB_TOKEN, CAROL_TOKEN, INSTALLATION_ID, ORG_ID,
    PUBLIC_REPO_ID,
};
use hubdouble_testkit::{Answer, GoGithub, Graphql, TestServer, contents_path, path_escape};
use serde_json::{Value, json};

const SHA_ACCEPT: &str = "application/vnd.github.v3.sha";
const RAW_ACCEPT: &str = "application/vnd.github.v3.raw";
const ZERO_OID: &str = "0000000000000000000000000000000000000000";
const APP_TOKEN: &str = "ghs_fixture_gitdata_contents_write";

async fn server() -> TestServer {
    TestServer::builder()
        .fixture()
        .register(hubdouble_api_gitdata::register)
        .start()
        .await
}

async fn graphql_server() -> TestServer {
    TestServer::builder()
        .fixture()
        .register(hubdouble_api_gitdata::register)
        .register(hubdouble_api_repos::register)
        .start()
        .await
}

fn alice(s: &TestServer) -> GoGithub {
    GoGithub::new(s).with_token(ALICE_TOKEN)
}

fn valid(operation_id: &str, a: &Answer) -> Value {
    serde_json::from_slice(&a.body)
        .unwrap_or_else(|e| panic!("{operation_id} {}: not JSON: {e}", a.status))
}

fn rev(s: &TestServer, repo: i64, name: &str) -> ObjectId {
    let services = s.server.services();
    services
        .store
        .view(|tx| Ok(services.git.resolve_ref(tx, repo, name)?))
        .unwrap()
}

fn head(s: &TestServer, name: &str) -> ObjectId {
    rev(s, PUBLIC_REPO_ID, name)
}

fn at(s: &TestServer, expr: &str) -> hubdouble_git::Object {
    let services = s.server.services();
    services
        .store
        .view(|tx| Ok(services.git.expression(tx, PUBLIC_REPO_ID, expr)?))
        .unwrap()
}

async fn get_with(client: &GoGithub, path: &str, headers: &[(&'static str, &str)]) -> Answer {
    let mut set = http::HeaderMap::new();
    for &(name, value) in headers {
        set.insert(
            http::HeaderName::from_static(name),
            http::HeaderValue::from_str(value).unwrap(),
        );
    }
    client.call_headers(Method::GET, path, set).await
}

fn commit_on(s: &TestServer, repo: i64, sender: i64, branch: &str, path: &str) -> ObjectId {
    let services = s.server.services();
    let git = &services.git;
    services
        .store
        .update(|tx| {
            let name = format!("refs/heads/{branch}");
            let parent = if let Ok(id) = git.resolve_ref(tx, repo, &name) {
                id
            } else {
                let main = git.resolve_ref(tx, repo, "refs/heads/main")?;
                git.update_ref(tx, repo, sender, &name, None, Some(main), false)?;
                main
            };
            let change = FileChange::Write {
                path: path.to_owned(),
                data: format!("{branch}\n").into_bytes().into(),
                mode: EntryMode::File,
            };
            Ok(git.commit(
                tx,
                repo,
                sender,
                branch,
                Some(parent),
                &[change],
                &format!("write {path}"),
                &Signature::seed(),
                &Signature::seed(),
                false,
            )?)
        })
        .unwrap()
}

fn shas(operation_id: &str, a: &Answer) -> Vec<String> {
    valid(operation_id, a)
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["sha"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn refs_read_by_name_and_by_prefix_and_an_empty_match_is_an_empty_list() {
    // Given
    let s = server().await;
    let client = alice(&s);
    let main = head(&s, "refs/heads/main");
    // When
    let one = client.get("repos/acme/public/git/ref/heads/main").await;
    let tags = client
        .get("repos/acme/public/git/matching-refs/tags/v")
        .await;
    let none = client
        .get("repos/acme/public/git/matching-refs/tags/agent/")
        .await;
    let missing = client.get("repos/acme/public/git/ref/heads/nope").await;
    // Then
    assert_eq!(one.status, StatusCode::OK);
    let one = valid("git/get-ref", &one);
    assert_eq!(one["ref"], "refs/heads/main");
    assert_eq!(
        one["url"],
        format!(
            "{}/repos/acme/public/git/refs/heads/main",
            client.base().trim_end_matches('/')
        )
    );
    assert_eq!(one["object"]["type"], "commit");
    assert_eq!(one["object"]["sha"], main.to_string());
    let tags = valid("git/list-matching-refs", &tags);
    assert_eq!(tags.as_array().unwrap().len(), 1);
    assert_eq!(tags[0]["ref"], "refs/tags/v1.0.0");
    assert_eq!(tags[0]["object"]["type"], "tag");
    assert_eq!(none.status, StatusCode::OK);
    assert_eq!(none.json(), json!([]));
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn an_annotated_tag_reads_back_with_the_commit_it_tags() {
    // Given
    let s = server().await;
    let client = alice(&s);
    let tag = head(&s, "refs/tags/v1.0.0");
    let main = head(&s, "refs/heads/main");
    // When
    let answer = client
        .get(&format!("repos/acme/public/git/tags/{tag}"))
        .await;
    let not_a_tag = client
        .get(&format!("repos/acme/public/git/tags/{main}"))
        .await;
    // Then
    let body = valid("git/get-tag", &answer);
    assert_eq!(body["sha"], tag.to_string());
    assert_eq!(body["tag"], "v1.0.0");
    assert_eq!(body["object"]["type"], "commit");
    assert_eq!(body["object"]["sha"], main.to_string());
    assert_eq!(not_a_tag.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_hidden_repository_is_not_found_and_a_read_role_cannot_write() {
    // Given
    let s = server().await;
    let bob = GoGithub::new(&s).with_token(BOB_TOKEN);
    let carol = GoGithub::new(&s).with_token(CAROL_TOKEN);
    let new_blob = json!({"content": "x", "encoding": "utf-8"});
    let branch = json!({"ref": "refs/heads/b", "sha": head(&s, "refs/heads/main").to_string()});
    // When
    let hidden = bob.get("repos/acme/private/git/ref/heads/main").await;
    let hidden_contents = bob.get("repos/acme/private/contents/README.md").await;
    let hidden_commits = bob.get("repos/acme/private/commits").await;
    let read = carol.get("repos/acme/private/git/ref/heads/main").await;
    let write = carol.post("repos/acme/private/git/blobs", &new_blob).await;
    let public_write = bob.post("repos/acme/public/git/refs", &branch).await;
    // Then
    assert_eq!(hidden.status, StatusCode::NOT_FOUND);
    assert_eq!(hidden_contents.status, StatusCode::NOT_FOUND);
    assert_eq!(hidden_commits.status, StatusCode::NOT_FOUND);
    assert_eq!(read.status, StatusCode::OK);
    assert_eq!(write.status, StatusCode::FORBIDDEN);
    assert_eq!(public_write.status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn a_blob_reads_back_as_json_and_as_raw_bytes() {
    // Given
    let s = server().await;
    let client = alice(&s);
    // When
    let made = client
        .post(
            "repos/acme/public/git/blobs",
            &json!({"content": "aGVsbG8K", "encoding": "base64"}),
        )
        .await;
    let bad = client
        .post(
            "repos/acme/public/git/blobs",
            &json!({"content": "x", "encoding": "rot13"}),
        )
        .await;
    let sha = made.json()["sha"].as_str().unwrap().to_owned();
    let path = format!("repos/acme/public/git/blobs/{sha}");
    let json_form = client.get(&path).await;
    let mut raw = Vec::new();
    for accept in ["application/vnd.github.raw", RAW_ACCEPT] {
        raw.push(get_with(&client, &path, &[("accept", accept)]).await);
    }
    let binary = client
        .post(
            "repos/acme/public/git/blobs",
            &json!({"content": "iVBORw0KGgoAAAANSUhEUg==", "encoding": "base64"}),
        )
        .await;
    let binary_path = format!(
        "repos/acme/public/git/blobs/{}",
        binary.json()["sha"].as_str().unwrap()
    );
    let binary_raw = get_with(
        &client,
        &binary_path,
        &[("accept", "application/vnd.github.raw")],
    )
    .await;
    // Then
    assert_eq!(made.status, StatusCode::CREATED);
    valid("git/create-blob", &made);
    assert_eq!(sha, "ce013625030ba8dba906f756967f9e9ca394464a");
    assert_eq!(bad.status, StatusCode::UNPROCESSABLE_ENTITY);
    let body = valid("git/get-blob", &json_form);
    assert_eq!(body["encoding"], "base64");
    assert_eq!(body["content"], "aGVsbG8K\n");
    assert_eq!(body["size"], 6);
    for raw in raw {
        assert_eq!(raw.status, StatusCode::OK);
        assert_eq!(raw.header("content-type"), "text/plain; charset=utf-8");
        assert_eq!(&raw.body[..], b"hello\n");
    }
    assert_eq!(binary_raw.status, StatusCode::OK);
    assert_eq!(
        binary_raw.header("content-type"),
        "application/octet-stream"
    );
    assert_eq!(&binary_raw.body[..4], b"\x89PNG");
}

#[tokio::test]
async fn a_tree_on_a_base_adds_nested_paths_and_deletes_null_shas() {
    // Given
    let s = server().await;
    let client = alice(&s);
    let base = at(&s, "feature:").id.to_string();
    let blob = client
        .post(
            "repos/acme/public/git/blobs",
            &json!({"content": "new\n", "encoding": "utf-8"}),
        )
        .await
        .json()["sha"]
        .clone();
    // When
    let made = client
        .post(
            "repos/acme/public/git/trees",
            &json!({"base_tree": base, "tree": [
                {"path": "docs/guide/a.md", "mode": "100644", "type": "blob", "sha": blob},
                {"path": "src/feature.txt", "mode": "100644", "type": "blob", "sha": null},
                {"path": "inline.txt", "mode": "100644", "type": "blob", "content": "inline\n"}
            ]}),
        )
        .await;
    let missing_blob = client
        .post(
            "repos/acme/public/git/trees",
            &json!({"tree": [{"path": "a", "mode": "100644", "type": "blob", "sha": ZERO_OID}]}),
        )
        .await;
    let tree = made.json()["sha"].as_str().unwrap().to_owned();
    let flat = client
        .get(&format!("repos/acme/public/git/trees/{tree}"))
        .await;
    let deep = client
        .get(&format!("repos/acme/public/git/trees/{tree}?recursive=1"))
        .await;
    let by_branch = client
        .get("repos/acme/public/git/trees/feature?recursive=1")
        .await;
    // Then
    assert_eq!(made.status, StatusCode::CREATED);
    valid("git/create-tree", &made);
    assert_eq!(missing_blob.status, StatusCode::UNPROCESSABLE_ENTITY);
    let paths = |a: &Answer| -> Vec<String> {
        valid("git/get-tree", a)["tree"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["path"].as_str().unwrap().to_owned())
            .collect()
    };
    assert_eq!(paths(&flat), ["README.md", "docs", "inline.txt"]);
    assert_eq!(
        paths(&deep),
        [
            "README.md",
            "docs",
            "docs/guide",
            "docs/guide/a.md",
            "inline.txt"
        ]
    );
    assert!(paths(&by_branch).contains(&"src/feature.txt".to_owned()));
    let entries = deep.json()["tree"].clone();
    assert_eq!(entries[1]["type"], "tree");
    assert_eq!(entries[3]["type"], "blob");
    assert_eq!(entries[3]["size"], 4);
}

#[tokio::test]
async fn a_tree_ish_with_a_path_answers_the_subtree_at_that_path() {
    // Given
    let s = server().await;
    let client = alice(&s);
    let commit = head(&s, "refs/heads/feature");
    let root = at(&s, "feature:").id;
    let src = at(&s, "feature:src").id;
    let blob = at(&s, "feature:src/feature.txt").id;
    // When
    let from_commit = client
        .get(&format!("repos/acme/public/git/trees/{commit}:src"))
        .await;
    let from_tree = client
        .get(&format!("repos/acme/public/git/trees/{root}:src/"))
        .await;
    let to_blob = client
        .get("repos/acme/public/git/trees/feature:src/feature.txt")
        .await;
    let a_blob = client
        .get(&format!("repos/acme/public/git/trees/{blob}"))
        .await;
    let missing = client.get("repos/acme/public/git/trees/feature:nope").await;
    // Then
    for answer in [&from_commit, &from_tree] {
        assert_eq!(answer.status, StatusCode::OK);
        let body = valid("git/get-tree", answer);
        assert_eq!(body["sha"], src.to_string());
        assert_eq!(body["tree"][0]["path"], "feature.txt");
    }
    assert_eq!(to_blob.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(a_blob.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_commit_and_a_ref_made_through_the_git_data_api_read_back() {
    // Given
    let s = server().await;
    let client = alice(&s);
    let main = head(&s, "refs/heads/main").to_string();
    let tree = at(&s, "main:").id.to_string();
    // When
    let made = client
        .post(
            "repos/acme/public/git/commits",
            &json!({"message": "m", "tree": tree, "parents": [main]}),
        )
        .await;
    let bad_tree = client
        .post(
            "repos/acme/public/git/commits",
            &json!({"message": "m", "tree": main}),
        )
        .await;
    let sha = made.json()["sha"].as_str().unwrap().to_owned();
    let work = json!({"ref": "refs/heads/work", "sha": sha});
    let created = client.post("repos/acme/public/git/refs", &work).await;
    let again = client.post("repos/acme/public/git/refs", &work).await;
    let bad_name = client
        .post(
            "repos/acme/public/git/refs",
            &json!({"ref": "work", "sha": sha}),
        )
        .await;
    let read = client.get("repos/acme/public/git/ref/heads/work").await;
    let fetched = client
        .get(&format!("repos/acme/public/git/commits/{sha}"))
        .await;
    let deleted = client
        .call(Method::DELETE, "repos/acme/public/git/refs/heads/work")
        .await;
    let gone = client.get("repos/acme/public/git/ref/heads/work").await;
    let deleted_again = client
        .call(Method::DELETE, "repos/acme/public/git/refs/heads/work")
        .await;
    // Then
    assert_eq!(made.status, StatusCode::CREATED);
    let body = valid("git/create-commit", &made);
    assert_eq!(body["tree"]["sha"], tree);
    assert_eq!(body["parents"][0]["sha"], main);
    assert_eq!(body["author"]["name"], "Alice");
    assert_eq!(body["author"]["email"], "alice@example.com");
    assert_eq!(body["verification"]["verified"], false);
    assert_eq!(body["verification"]["reason"], "unsigned");
    assert_eq!(bad_tree.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(created.status, StatusCode::CREATED);
    let url = format!(
        "{}/repos/acme/public/git/refs/heads/work",
        client.base().trim_end_matches('/')
    );
    assert_eq!(valid("git/create-ref", &created)["url"], url);
    assert_eq!(created.header("location"), url);
    assert_eq!(again.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(bad_name.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(read.json()["object"]["sha"], sha);
    assert_eq!(valid("git/get-commit", &fetched)["message"], "m");
    assert_eq!(deleted.status, StatusCode::NO_CONTENT);
    assert_eq!(gone.status, StatusCode::NOT_FOUND);
    assert_eq!(deleted_again.status, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn a_commit_the_app_makes_without_an_author_is_web_flow_signed() {
    // Given
    let s = server().await;
    s.server
        .services()
        .store
        .update(|tx| {
            let mut installation =
                (**tx.table::<Installations>().get(INSTALLATION_ID).unwrap()).clone();
            installation
                .permissions
                .insert("contents".to_owned(), Access::Write);
            tx.table_mut::<Installations>().replace(installation)?;
            let id = tx.next_id();
            tx.table_mut::<Tokens>().insert(Token {
                id,
                token: APP_TOKEN.into(),
                kind: TokenKind::Installation,
                app_id: Some(APP_ID),
                installation_id: Some(INSTALLATION_ID),
                user_id: None,
                repo_ids: None,
                permissions: BTreeMap::from([("contents".to_owned(), Access::Write)]),
                scopes: vec![],
                expires_at: None,
                revoked: false,
                owner_seeded: false,
                created_at: s.clock.now(),
            })?;
            Ok(())
        })
        .unwrap();
    let app = GoGithub::new(&s).with_token(APP_TOKEN);
    let commit = json!({
        "message": "m",
        "tree": at(&s, "main:").id.to_string(),
        "parents": [head(&s, "refs/heads/main").to_string()],
    });
    let mut authored = commit.clone();
    authored["author"] = json!({"name": "A", "email": "a@example.com"});
    // When
    let by_app = app.post("repos/acme/public/git/commits", &commit).await;
    let by_app_as_author = app.post("repos/acme/public/git/commits", &authored).await;
    let by_user = alice(&s)
        .post("repos/acme/public/git/commits", &commit)
        .await;
    // Then
    let signed = valid("git/create-commit", &by_app);
    assert_eq!(signed["verification"]["verified"], true);
    assert_eq!(signed["verification"]["reason"], "valid");
    let fetched = app
        .get(&format!(
            "repos/acme/public/git/commits/{}",
            signed["sha"].as_str().unwrap()
        ))
        .await;
    assert_eq!(fetched.json()["verification"]["verified"], true);
    for unsigned in [&by_app_as_author, &by_user] {
        let body = valid("git/create-commit", unsigned);
        assert_eq!(body["verification"]["reason"], "unsigned");
    }
}

#[tokio::test]
async fn contents_answer_a_file_a_directory_the_root_and_raw_bytes_at_a_ref() {
    // Given
    let s = server().await;
    let client = alice(&s);
    let file_path = format!(
        "repos/acme/public/contents/{}?ref=feature",
        contents_path("src/feature.txt")
    );
    // When
    let file = client.get(&file_path).await;
    let dir = client
        .get("repos/acme/public/contents/src?ref=feature")
        .await;
    let root = client
        .get(&format!("repos/acme/public/contents/{}", contents_path("")))
        .await;
    let raw = get_with(
        &client,
        "repos/acme/public/contents/README.md",
        &[("accept", "application/vnd.github.raw")],
    )
    .await;
    let missing_on_main = client
        .get("repos/acme/public/contents/src/feature.txt")
        .await;
    let missing_ref = client
        .get("repos/acme/public/contents/README.md?ref=nope")
        .await;
    // Then
    let file = valid("repos/get-content", &file);
    assert_eq!(file["type"], "file");
    assert_eq!(file["path"], "src/feature.txt");
    assert_eq!(file["encoding"], "base64");
    assert_eq!(file["content"], "ZmVhdHVyZQo=\n");
    let dir = valid("repos/get-content", &dir);
    assert_eq!(dir[0]["name"], "feature.txt");
    assert_eq!(dir[0]["type"], "file");
    let root = valid("repos/get-content", &root);
    assert_eq!(root.as_array().unwrap().len(), 1);
    assert_eq!(root[0]["path"], "README.md");
    assert_eq!(raw.status, StatusCode::OK);
    assert_eq!(&raw.body[..], b"# public\n");
    assert_eq!(missing_on_main.status, StatusCode::NOT_FOUND);
    assert_eq!(missing_ref.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn contents_object_media_wraps_directory_entries() {
    let s = server().await;
    let client = alice(&s);
    for (path, reference, name, entry) in [
        ("", "main", "", "README.md"),
        ("src", "feature", "src", "feature.txt"),
    ] {
        let answer = get_with(
            &client,
            &format!("repos/acme/public/contents/{path}?ref={reference}"),
            &[("accept", "application/vnd.github.object+json")],
        )
        .await;
        assert_eq!(answer.status, StatusCode::OK);
        assert_eq!(
            answer.header("content-type"),
            "application/json; charset=utf-8"
        );
        let directory = answer.json();
        assert_eq!(directory["type"], "dir");
        assert_eq!(directory["size"], 0);
        assert_eq!(directory["name"], name);
        assert_eq!(directory["path"], path);
        assert_eq!(directory["entries"][0]["name"], entry);
        assert_eq!(directory["download_url"], Value::Null);
    }
}

#[tokio::test]
async fn contents_raw_json_media_preserves_the_type_and_bytes() {
    let s = server().await;
    let client = alice(&s);
    for media in [
        "application/vnd.github.raw+json",
        "application/vnd.github.v3.raw+json",
        "application/vnd.github.v3.raw",
    ] {
        let answer = get_with(
            &client,
            "repos/acme/public/contents/README.md",
            &[("accept", media)],
        )
        .await;
        assert_eq!(answer.status, StatusCode::OK);
        assert_eq!(
            answer.header("content-type"),
            format!("{media}; charset=utf-8")
        );
        assert_eq!(answer.body.as_ref(), b"# public\n");
    }
}

#[tokio::test]
async fn commits_list_by_branch_and_by_path_with_pages() {
    // Given
    let s = server().await;
    let client = alice(&s);
    let feature = head(&s, "refs/heads/feature").to_string();
    let main = head(&s, "refs/heads/main").to_string();
    // When
    let first = client
        .get("repos/acme/public/commits?per_page=1&sha=feature")
        .await;
    let second = client
        .get("repos/acme/public/commits?page=2&per_page=1&sha=feature")
        .await;
    let by_path = client
        .get("repos/acme/public/commits?path=src%2Ffeature.txt&per_page=1&sha=feature")
        .await;
    let readme = client
        .get("repos/acme/public/commits?path=README.md&sha=feature")
        .await;
    let missing = client.get("repos/acme/public/commits?sha=nope").await;
    // Then
    assert_eq!(shas("repos/list-commits", &first), [feature.as_str()]);
    assert!(first.header("link").contains("rel=\"next\""));
    assert_eq!(shas("repos/list-commits", &second), [main.as_str()]);
    assert_eq!(shas("repos/list-commits", &by_path), [feature.as_str()]);
    assert_eq!(shas("repos/list-commits", &readme), [main.as_str()]);
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_commit_reads_with_its_files_and_as_a_sha_with_a_conditional_answer() {
    // Given
    let s = server().await;
    let client = alice(&s);
    let feature = head(&s, "refs/heads/feature").to_string();
    let main = head(&s, "refs/heads/main");
    let etag = format!("\"{feature}\"");
    // When
    let full = client.get("repos/acme/public/commits/feature").await;
    let root = client
        .get(&format!("repos/acme/public/commits/{main}"))
        .await;
    let paged = client
        .get("repos/acme/public/commits/feature?per_page=1")
        .await;
    let sha = get_with(
        &client,
        "repos/acme/public/commits/feature",
        &[("accept", SHA_ACCEPT)],
    )
    .await;
    let unchanged = get_with(
        &client,
        "repos/acme/public/commits/feature",
        &[("accept", SHA_ACCEPT), ("if-none-match", &etag)],
    )
    .await;
    let missing = client.get("repos/acme/public/commits/nope").await;
    // Then
    let body = valid("repos/get-commit", &full);
    assert_eq!(body["sha"], feature);
    assert_eq!(body["parents"][0]["sha"], main.to_string());
    assert_eq!(body["files"][0]["filename"], "src/feature.txt");
    assert_eq!(body["files"][0]["status"], "added");
    assert_eq!(body["files"][0]["additions"], 1);
    assert_eq!(body["stats"]["total"], 1);
    let root = valid("repos/get-commit", &root);
    assert_eq!(root["files"][0]["filename"], "README.md");
    assert_eq!(root["files"][0]["patch"], "@@ -0,0 +1 @@\n+# public");
    let paged = valid("repos/get-commit", &paged);
    assert_eq!(paged["files"].as_array().unwrap().len(), 1);
    assert_eq!(sha.status, StatusCode::OK);
    assert_eq!(
        sha.header("content-type"),
        "application/vnd.github.v3.sha; charset=utf-8"
    );
    assert_eq!(std::str::from_utf8(&sha.body).unwrap(), feature);
    assert_eq!(unchanged.status, StatusCode::NOT_MODIFIED);
    assert_eq!(missing.status, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn compare_counts_commits_both_ways_and_pages_its_commits() {
    // Given
    let s = server().await;
    let client = alice(&s);
    let feature = head(&s, "refs/heads/feature").to_string();
    let main = head(&s, "refs/heads/main").to_string();
    let develop = commit_on(&s, PUBLIC_REPO_ID, ALICE_ID, "develop", "dev.txt");
    commit_on(&s, PUBLIC_REPO_ID, ALICE_ID, "feature", "more.txt");
    // When
    let page_one = client
        .get("repos/acme/public/compare/develop...feature?per_page=1")
        .await;
    let page_two = client
        .get("repos/acme/public/compare/develop...feature?page=2&per_page=1")
        .await;
    let by_sha = client
        .get(&format!("repos/acme/public/compare/{develop}...{feature}"))
        .await;
    let two_dots = client
        .get("repos/acme/public/compare/develop..feature")
        .await;
    let same = client.get("repos/acme/public/compare/main...main").await;
    let behind = client.get("repos/acme/public/compare/develop...main").await;
    let missing = client.get("repos/acme/public/compare/main...nope").await;
    // Then
    let one = valid("repos/compare-commits", &page_one);
    assert_eq!(one["status"], "diverged");
    assert_eq!(one["ahead_by"], 2);
    assert_eq!(one["behind_by"], 1);
    assert_eq!(one["total_commits"], 2);
    assert_eq!(one["merge_base_commit"]["sha"], main);
    assert_eq!(one["commits"].as_array().unwrap().len(), 1);
    assert_eq!(one["commits"][0]["sha"], feature);
    let files: Vec<&str> = one["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["filename"].as_str().unwrap())
        .collect();
    assert_eq!(files, ["more.txt", "src/feature.txt"]);
    assert!(page_one.header("link").contains("rel=\"next\""));
    let two = valid("repos/compare-commits", &page_two);
    assert_eq!(two["commits"].as_array().unwrap().len(), 1);
    assert_ne!(two["commits"][0]["sha"], feature);
    assert!(two.get("files").is_none());
    assert_eq!(valid("repos/compare-commits", &by_sha)["behind_by"], 1);
    assert_eq!(valid("repos/compare-commits", &two_dots)["ahead_by"], 2);
    assert_eq!(valid("repos/compare-commits", &same)["status"], "identical");
    assert_eq!(valid("repos/compare-commits", &behind)["status"], "behind");
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn unpaged_compare_keeps_the_tail_in_chronological_order() {
    let commits: Vec<_> = (1..=251)
        .map(|n| json!({"branch":"feature", "message": n.to_string(), "files":{"file": n.to_string()}}))
        .collect();
    let s = TestServer::builder()
        .fixture()
        .register(hubdouble_api_gitdata::register)
        .entry(json!({
            "kind":"repository", "owner":"acme", "name":"long-history",
            "files":{"file":"base"}, "commits":commits,
        }))
        .start()
        .await;
    let client = alice(&s);
    let path = "repos/acme/long-history/compare/main...feature";
    let answer = client.get(path).await;
    assert_eq!(answer.status, StatusCode::OK);
    let body = valid("repos/compare-commits", &answer);
    assert_eq!(body["total_commits"], 251);
    let shown = body["commits"].as_array().unwrap();
    assert_eq!(shown.len(), 250);
    for (commit, n) in shown.iter().zip(2..=251) {
        assert_eq!(commit["commit"]["message"], n.to_string());
    }
    let head = client.get("repos/acme/long-history/commits/feature").await;
    assert_eq!(shown[249]["sha"], head.json()["sha"]);
    let first = client.get(&format!("{path}?per_page=1")).await.json();
    assert_eq!(first["commits"][0]["commit"]["message"], "1");
}

#[tokio::test]
async fn a_head_pushed_only_to_a_fork_compares_through_the_parent() {
    // Given
    let s = TestServer::builder()
        .fixture()
        .entry(json!({"kind": "repository", "owner": BOB, "name": "public",
                      "fork_of": "acme/public"}))
        .register(hubdouble_api_gitdata::register)
        .start()
        .await;
    let fork = s
        .server
        .services()
        .store
        .view(|tx| {
            Ok(tx
                .table::<Repos>()
                .iter()
                .find(|r| r.fork_of == Some(PUBLIC_REPO_ID))
                .map(|r| r.id))
        })
        .unwrap()
        .unwrap();
    let pushed = commit_on(&s, fork, BOB_ID, "fix", "fix.txt");
    let client = alice(&s);
    // When
    let by_ref = client
        .get(&format!("repos/acme/public/compare/main...{BOB}:fix"))
        .await;
    let by_repo = client
        .get(&format!(
            "repos/acme/public/compare/main...{BOB}:public:fix"
        ))
        .await;
    let by_sha = client
        .get(&format!("repos/acme/public/compare/main...{pushed}"))
        .await;
    // Then
    for answer in [&by_ref, &by_repo, &by_sha] {
        let body = valid("repos/compare-commits", answer);
        assert_eq!(body["ahead_by"], 1);
        assert_eq!(body["commits"][0]["sha"], pushed.to_string());
        assert_eq!(body["files"][0]["filename"], "fix.txt");
    }
}

#[tokio::test]
async fn merges_answer_created_nothing_to_merge_and_conflict() {
    // Given
    let s = server().await;
    let client = alice(&s);
    let develop = head(&s, "refs/heads/develop").to_string();
    let feature = head(&s, "refs/heads/feature").to_string();
    commit_on(&s, PUBLIC_REPO_ID, ALICE_ID, "left", "same.txt");
    commit_on(&s, PUBLIC_REPO_ID, ALICE_ID, "right", "same.txt");
    let merge =
        |base: &str, head: &str| json!({"base": base, "head": head, "commit_message": "merge it"});
    // When
    let made = client
        .post("repos/acme/public/merges", &merge("develop", "feature"))
        .await;
    let again = client
        .post("repos/acme/public/merges", &merge("develop", "feature"))
        .await;
    let first = client
        .post("repos/acme/public/merges", &merge("main", "left"))
        .await;
    let conflict = client
        .post("repos/acme/public/merges", &merge("main", "right"))
        .await;
    let no_base = client
        .post("repos/acme/public/merges", &merge("nope", "feature"))
        .await;
    let no_head = client
        .post("repos/acme/public/merges", &merge("develop", "nope"))
        .await;
    let denied = GoGithub::new(&s)
        .with_token(BOB_TOKEN)
        .post("repos/acme/public/merges", &merge("main", "feature"))
        .await;
    // Then
    assert_eq!(made.status, StatusCode::CREATED);
    let body = valid("repos/merge", &made);
    assert_eq!(body["parents"][0]["sha"], develop);
    assert_eq!(body["parents"][1]["sha"], feature);
    assert_eq!(body["commit"]["message"], "merge it");
    assert_eq!(head(&s, "refs/heads/develop").to_string(), body["sha"]);
    assert_eq!(again.status, StatusCode::NO_CONTENT);
    assert_eq!(first.status, StatusCode::CREATED);
    assert_eq!(conflict.status, StatusCode::CONFLICT);
    assert_eq!(no_base.status, StatusCode::NOT_FOUND);
    assert_eq!(no_head.status, StatusCode::NOT_FOUND);
    assert_eq!(denied.status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn a_branch_reads_with_its_protection_and_a_missing_one_is_branch_not_found() {
    // Given
    let s = server().await;
    commit_on(&s, PUBLIC_REPO_ID, ALICE_ID, "release/v1", "r.txt");
    s.server
        .services()
        .store
        .update_quiet(|tx| {
            let id = tx.next_id();
            tx.table_mut::<Rulesets>()
                .insert(Ruleset {
                    id,
                    node_id: node_id::entity(node_id::RULESET, id),
                    source_type: RulesetSource::Repository,
                    source_id: PUBLIC_REPO_ID,
                    repo_id: Some(PUBLIC_REPO_ID),
                    name: "releases".to_owned(),
                    target: RulesetTarget::Branch,
                    enforcement: Enforcement::Active,
                    bypass_actors: Vec::new(),
                    conditions: Some(json!({"ref_name": {
                        "include": ["refs/heads/release/*"], "exclude": []}})),
                    rules: Vec::new(),
                    created_at: Timestamp::SEED,
                    updated_at: Timestamp::SEED,
                })
                .unwrap();
            Ok(())
        })
        .unwrap();
    let client = alice(&s);
    // When
    let main = client.get("repos/acme/public/branches/main").await;
    let release = client
        .get(&format!(
            "repos/acme/public/branches/{}",
            path_escape("release/v1")
        ))
        .await;
    let missing = client.get("repos/acme/public/branches/nope").await;
    // Then
    let main = valid("repos/get-branch", &main);
    assert_eq!(main["name"], "main");
    assert_eq!(main["protected"], false);
    assert_eq!(
        main["commit"]["sha"],
        head(&s, "refs/heads/main").to_string()
    );
    let release = valid("repos/get-branch", &release);
    assert_eq!(release["name"], "release/v1");
    assert_eq!(release["protected"], true);
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
    assert_eq!(missing.json()["message"], "Branch not found");
}

#[tokio::test]
async fn an_org_ruleset_protects_only_the_repositories_and_branches_its_conditions_name() {
    // Given
    let s = server().await;
    commit_on(&s, PUBLIC_REPO_ID, ALICE_ID, "release/v1", "r.txt");
    commit_on(&s, PUBLIC_REPO_ID, ALICE_ID, "release/v10", "r.txt");
    let org_ruleset = |id: i64, conditions: Value| Ruleset {
        id,
        node_id: node_id::entity(node_id::RULESET, id),
        source_type: RulesetSource::Organization,
        source_id: ORG_ID,
        repo_id: None,
        name: format!("org-{id}"),
        target: RulesetTarget::Branch,
        enforcement: Enforcement::Active,
        bypass_actors: Vec::new(),
        conditions: Some(conditions),
        rules: Vec::new(),
        created_at: Timestamp::SEED,
        updated_at: Timestamp::SEED,
    };
    s.server
        .services()
        .store
        .update_quiet(|tx| {
            let mut rulesets = tx.table_mut::<Rulesets>();
            rulesets
                .insert(org_ruleset(
                    9001,
                    json!({"ref_name": {"include": ["~ALL"]},
                           "repository_name": {"include": ["other"]}}),
                ))
                .unwrap();
            rulesets
                .insert(org_ruleset(
                    9002,
                    json!({"ref_name": {"include": ["refs/heads/release/v?"]},
                           "repository_name": {"include": ["public"]}}),
                ))
                .unwrap();
            Ok(())
        })
        .unwrap();
    let client = alice(&s);
    // When
    let branch = |name: &str| format!("repos/acme/public/branches/{}", path_escape(name));
    let main = client.get(&branch("main")).await.json();
    let v1 = client.get(&branch("release/v1")).await.json();
    let v10 = client.get(&branch("release/v10")).await.json();
    // Then
    assert_eq!(main["protected"], false);
    assert_eq!(v1["protected"], true);
    assert_eq!(v10["protected"], false);
}

fn document(name: &str) -> String {
    std::fs::read_to_string(format!(
        "{}/../../docs/inventory/graphql/{name}.graphql",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
}

async fn query(s: &TestServer, token: &str, name: &str, variables: Value) -> Value {
    let answer = Graphql::new(GoGithub::new(s).with_token(token))
        .query(&document(name), &variables)
        .await;
    assert_eq!(answer.status, StatusCode::OK);
    answer.json()
}

fn data(body: &Value) -> &Value {
    assert!(body.get("errors").is_none(), "errors: {body}");
    &body["data"]
}

fn public(extra: Value) -> Value {
    let mut vars = json!({"owner": "acme", "name": "public"});
    if let (Some(v), Value::Object(e)) = (vars.as_object_mut(), extra) {
        v.extend(e);
    }
    vars
}

fn tag_at(s: &TestServer, name: &str, target: ObjectId) {
    let services = s.server.services();
    let git = &services.git;
    services
        .store
        .update(|tx| {
            let tag = git.write_tag(
                tx,
                PUBLIC_REPO_ID,
                &Tag {
                    target,
                    target_kind: ObjectKind::Commit,
                    name: name.to_owned(),
                    tagger: Some(Signature::seed()),
                    message: name.to_owned(),
                    gpg_signature: None,
                },
            )?;
            let full = format!("refs/tags/{name}");
            git.update_ref(tx, PUBLIC_REPO_ID, ALICE_ID, &full, None, Some(tag), false)?;
            Ok(())
        })
        .unwrap();
}

#[tokio::test]
async fn head_default_head_and_workflows_read_refs_and_objects() {
    // Given
    let s = graphql_server().await;
    let main = head(&s, "refs/heads/main").to_string();
    let feature = head(&s, "refs/heads/feature").to_string();
    // When
    let by_ref = query(
        &s,
        ALICE_TOKEN,
        "head",
        public(json!({"ref": "refs/heads/feature"})),
    )
    .await;
    let missing = query(
        &s,
        ALICE_TOKEN,
        "head",
        public(json!({"ref": "refs/heads/nope"})),
    )
    .await;
    let default_head = query(&s, ALICE_TOKEN, "defaultHead", public(json!({}))).await;
    let workflows = query(
        &s,
        ALICE_TOKEN,
        "workflows",
        public(json!({"before": format!("{main}:README.md"),
                      "after": "feature:src/feature.txt"})),
    )
    .await;
    // Then
    assert_eq!(data(&by_ref)["repository"]["ref"]["target"]["oid"], feature);
    assert_eq!(data(&missing)["repository"]["ref"], Value::Null);
    let d = &data(&default_head)["repository"]["defaultBranchRef"];
    assert_eq!(d["name"], "main");
    assert_eq!(d["target"]["oid"], main);
    let w = &data(&workflows)["repository"];
    assert_eq!(w["before"]["oid"], at(&s, "main:README.md").id.to_string());
    assert_eq!(
        w["after"]["oid"],
        at(&s, "feature:src/feature.txt").id.to_string()
    );
}

#[tokio::test]
async fn tags_and_the_releases_probe_list_refs_under_a_prefix_and_peel_tags() {
    // Given
    let s = graphql_server().await;
    let main = head(&s, "refs/heads/main");
    let feature = head(&s, "refs/heads/feature");
    tag_at(&s, "agent/v5.0.0", main);
    tag_at(&s, "agent/v5.1.0", feature);
    let tag = head(&s, "refs/tags/agent/v5.1.0").to_string();
    // When
    let tags = query(&s, ALICE_TOKEN, "tags", public(json!({}))).await;
    let probe = query(
        &s,
        ALICE_TOKEN,
        "releasesProbe",
        public(json!({"branch": "refs/heads/main"})),
    )
    .await;
    let version_ref = query(
        &s,
        ALICE_TOKEN,
        "versionRef",
        public(json!({"r0": "refs/tags/agent/v5.1.0"})),
    )
    .await;
    let peel = query(&s, ALICE_TOKEN, "peelTags", public(json!({}))).await;
    // Then
    let t = &data(&tags)["repository"]["refs"];
    assert_eq!(t["pageInfo"]["hasNextPage"], false);
    let nodes = t["nodes"].as_array().unwrap();
    assert_eq!(nodes.len(), 2);
    assert_eq!(nodes[0]["name"], "v5.0.0");
    assert_eq!(nodes[1]["name"], "v5.1.0");
    assert_eq!(nodes[0]["target"]["target"]["oid"], main.to_string());
    assert_eq!(nodes[1]["target"]["oid"], tag);
    assert_eq!(nodes[1]["target"]["target"]["oid"], feature.to_string());
    let p = &data(&probe)["repository"];
    assert_eq!(p["refs"]["totalCount"], 2);
    assert_eq!(p["refs"]["nodes"].as_array().unwrap().len(), 1);
    assert_eq!(p["main"]["target"]["oid"], main.to_string());
    let v = &data(&version_ref)["repository"]["r0"]["target"];
    assert_eq!(v["oid"], tag);
    assert_eq!(v["target"]["oid"], feature.to_string());
    assert_eq!(data(&peel)["repository"]["t0"], Value::Null);
}

#[tokio::test]
async fn blob_and_tree_expressions_read_text_and_entries() {
    // Given
    let s = graphql_server().await;
    let feature = head(&s, "refs/heads/feature").to_string();
    // When
    let blobs = query(
        &s,
        ALICE_TOKEN,
        "versionFiles",
        public(json!({"e0": format!("{feature}:README.md"),
                      "e1": format!("{feature}:nope.md")})),
    )
    .await;
    let trees = query(
        &s,
        ALICE_TOKEN,
        "holdsFiles",
        public(json!({"e0": format!("{feature}:"), "e1": format!("{feature}:src")})),
    )
    .await;
    let unknown = query(&s, ALICE_TOKEN, "filesAt", public(json!({}))).await;
    // Then
    let b = &data(&blobs)["repository"];
    assert_eq!(b["f0"]["text"], "# public\n");
    assert_eq!(b["f0"]["isTruncated"], false);
    assert_eq!(b["f1"], Value::Null);
    let t = &data(&trees)["repository"];
    assert_eq!(
        t["t0"]["entries"],
        json!([{"name": "README.md", "mode": 33188}, {"name": "src", "mode": 16384}])
    );
    assert_eq!(
        t["t1"]["entries"],
        json!([{"name": "feature.txt", "mode": 33188}])
    );
    let u = &data(&unknown)["repository"];
    assert_eq!(u["t0"], Value::Null);
    assert_eq!(u["t1"], Value::Null);
}

#[tokio::test]
async fn create_commit_on_branch_writes_a_signed_commit_on_the_expected_head_only() {
    // Given
    let s = graphql_server().await;
    let main = head(&s, "refs/heads/main");
    let input = |expected: &str| {
        json!({"input": {
            "branch": {"repositoryNameWithOwner": "acme/public", "branchName": "main"},
            "expectedHeadOid": expected,
            "message": {"headline": "add a", "body": "why"},
            "fileChanges": {"additions": [{"path": "a.txt", "contents": "YQo="}],
                            "deletions": [{"path": "README.md"}]}
        }})
    };
    // When
    let made = query(&s, ALICE_TOKEN, "createCommit", input(&main.to_string())).await;
    let new_head = head(&s, "refs/heads/main").to_string();
    let stale = query(&s, ALICE_TOKEN, "createCommit", input(&main.to_string())).await;
    let denied = query(&s, BOB_TOKEN, "createCommit", input(&new_head)).await;
    // Then
    let c = &data(&made)["createCommitOnBranch"]["commit"];
    assert_eq!(c["oid"], new_head);
    assert_eq!(c["signature"]["state"], "VALID");
    let tree = at(&s, &format!("{new_head}:"));
    assert_eq!(c["tree"]["oid"], tree.id.to_string());
    let ObjectData::Tree(entries) = tree.data else {
        panic!("{new_head}: is not a tree");
    };
    let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, ["a.txt"]);
    let ObjectData::Commit(commit) = at(&s, &new_head).data else {
        panic!("{new_head} is not a commit");
    };
    assert_eq!(commit.message, "add a\n\nwhy");
    assert_eq!(commit.parents, [main]);
    assert!(stale.get("errors").is_some(), "{stale}");
    assert!(denied.get("errors").is_some(), "{denied}");
    assert_eq!(head(&s, "refs/heads/main").to_string(), new_head);
}

#[tokio::test]
async fn update_refs_moves_every_ref_or_none() {
    // Given
    let s = graphql_server().await;
    let main = head(&s, "refs/heads/main").to_string();
    let feature = head(&s, "refs/heads/feature").to_string();
    let repository_id = s
        .server
        .services()
        .store
        .view(|tx| {
            Ok(tx
                .table::<Repos>()
                .get(PUBLIC_REPO_ID)
                .map(|r| r.node_id.clone()))
        })
        .unwrap()
        .unwrap();
    let input = |updates: Value| {
        json!({"input": {"repositoryId": repository_id, "refUpdates": updates,
                         "clientMutationId": "c1"}})
    };
    let move_develop = json!({"name": "refs/heads/develop", "beforeOid": main, "afterOid": feature, "force": false});
    // When
    let stale = query(
        &s,
        ALICE_TOKEN,
        "updateRefs",
        input(json!([move_develop,
            {"name": "refs/heads/feature", "beforeOid": main, "afterOid": main, "force": true}])),
    )
    .await;
    let denied = query(&s, BOB_TOKEN, "updateRefs", input(json!([move_develop]))).await;
    let develop_after_failures = head(&s, "refs/heads/develop").to_string();
    let moved = query(
        &s,
        ALICE_TOKEN,
        "updateRefs",
        input(json!([move_develop,
            {"name": "refs/heads/made", "beforeOid": ZERO_OID, "afterOid": main}])),
    )
    .await;
    // Then
    assert!(stale.get("errors").is_some(), "{stale}");
    assert!(denied.get("errors").is_some(), "{denied}");
    assert_eq!(develop_after_failures, main);
    assert_eq!(data(&moved)["updateRefs"]["clientMutationId"], "c1");
    assert_eq!(head(&s, "refs/heads/develop").to_string(), feature);
    assert_eq!(head(&s, "refs/heads/made").to_string(), main);
}
