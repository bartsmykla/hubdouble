use http::{HeaderMap, HeaderValue, Method, StatusCode, header::ACCEPT};
use hubdouble_git::{EntryMode, FileChange, Signature};
use hubdouble_testkit::{
    GoGithub, TestServer,
    fixture::{ALICE_ID, ALICE_TOKEN, PUBLIC_REPO_ID},
};

#[tokio::test]
async fn contents_resolves_symlinks_at_the_requested_ref() {
    let s = TestServer::builder()
        .fixture()
        .register(hubdouble_api_gitdata::register)
        .start()
        .await;
    let services = s.server.services();
    let main = services
        .store
        .view(|tx| {
            Ok(services
                .git
                .resolve_ref(tx, PUBLIC_REPO_ID, "refs/heads/main")?)
        })
        .unwrap();
    let commit = services
        .store
        .update_quiet(|tx| {
            let changes = [
                ("README.md", "changed target", EntryMode::File),
                ("docs/link", "../README.md", EntryMode::Symlink),
                ("docs/outside", "../../README.md", EntryMode::Symlink),
                ("docs/loop", "loop", EntryMode::Symlink),
            ]
            .map(|(path, data, mode)| FileChange::Write {
                path: path.into(),
                data: data.as_bytes().into(),
                mode,
            });
            Ok(services.git.commit(
                tx,
                PUBLIC_REPO_ID,
                ALICE_ID,
                "main",
                Some(main),
                &changes,
                "add symlinks",
                &Signature::seed(),
                &Signature::seed(),
                false,
            )?)
        })
        .unwrap();
    let client = GoGithub::new(&s).with_token(ALICE_TOKEN);
    let path = format!("repos/acme/public/contents/docs/link?ref={commit}");
    let answer = client.get(&path).await;
    assert_eq!(answer.status, StatusCode::OK);
    let file = answer.json();
    assert_eq!(file["type"], "file");
    assert_eq!(file["path"], "docs/link");
    assert_eq!(
        file["sha"],
        services
            .store
            .view(|tx| Ok(services
                .git
                .expression(tx, PUBLIC_REPO_ID, &format!("{commit}:docs/link"))?
                .id))
            .unwrap()
            .to_string()
    );
    assert_eq!(file["size"], 14);
    assert_eq!(
        client
            .call_headers(
                Method::GET,
                &path,
                HeaderMap::from_iter([(
                    ACCEPT,
                    HeaderValue::from_static("application/vnd.github.v3.raw")
                ),])
            )
            .await
            .body,
        "changed target"
    );
    for (name, target) in [("outside", "../../README.md"), ("loop", "loop")] {
        let answer = client
            .get(&format!("repos/acme/public/contents/docs/{name}"))
            .await;
        assert_eq!(answer.status, StatusCode::OK);
        let link = answer.json();
        assert_eq!(link["type"], "symlink");
        assert_eq!(link["target"], target);
        assert!(link.get("content").is_none());
    }
    assert_eq!(
        client
            .get(&format!("repos/acme/public/contents/docs/link?ref={main}"))
            .await
            .status,
        StatusCode::NOT_FOUND
    );
}
