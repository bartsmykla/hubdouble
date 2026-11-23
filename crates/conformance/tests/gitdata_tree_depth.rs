use http::StatusCode;
use hubdouble_git::{EntryMode, GitError, TreeEntry};
use hubdouble_testkit::fixture::{ALICE_TOKEN, PUBLIC_REPO_ID};
use hubdouble_testkit::{GoGithub, TestServer};
use serde_json::json;

#[tokio::test]
async fn create_tree_refuses_paths_and_subtrees_past_the_depth_limit() {
    let s = TestServer::builder()
        .fixture()
        .register(hubdouble_api_gitdata::register)
        .start()
        .await;
    let client = GoGithub::new(&s).with_token(ALICE_TOKEN);
    let route = "repos/acme/public/git/trees";
    let create = |path: String| {
        let client = &client;
        async move {
            client
                .post(
                    route,
                    &json!({"tree": [{"path": path, "mode": "100644", "content": "data"}]}),
                )
                .await
        }
    };
    let at_limit = create("d/".repeat(50) + "file").await;
    assert_eq!(at_limit.status, StatusCode::CREATED);
    let over_limit = create("d/".repeat(51) + "file").await;
    assert_eq!(over_limit.status, StatusCode::UNPROCESSABLE_ENTITY);

    let sha = at_limit.json()["sha"].clone();
    let attached = client
        .post(
            route,
            &json!({"tree": [{"path": "extra", "mode": "040000", "sha": sha}]}),
        )
        .await;
    assert_eq!(attached.status, StatusCode::UNPROCESSABLE_ENTITY);

    let services = s.server.services();
    let id = sha.as_str().unwrap().parse().unwrap();
    let wrapped = services.store.view(|tx| {
        Ok(services.git.write_tree(
            tx,
            PUBLIC_REPO_ID,
            &[TreeEntry {
                name: "extra".to_owned(),
                mode: EntryMode::Tree,
                id,
            }],
        ))
    });
    assert!(matches!(wrapped.unwrap(), Err(GitError::Invalid(_))));
}
