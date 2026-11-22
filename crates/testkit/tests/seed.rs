use hubdouble_git::ObjectData;
use hubdouble_store::Repos;
use hubdouble_testkit::TestServer;
use hubdouble_testkit::fixture::{ORG_ID, PRIVATE_REPO_ID, PUBLIC_REPO_ID};
use serde_json::json;

#[tokio::test]
async fn the_seed_writes_its_history_into_the_git_store() {
    // Given
    let server = TestServer::builder()
        .fixture()
        .entry(
            json!({"kind": "repository", "owner": "acme", "name": "pins",
                      "files": {"pin.txt": "{{sha \"acme/public\" \"v1.0.0\"}}"}}),
        )
        .start()
        .await;
    let services = server.server.services().clone();
    let git = &services.git;
    // When
    services
        .store
        .view(|tx| {
            let r = |repo, name| git.resolve_ref(tx, repo, name).unwrap();
            let object = |repo, name| git.object(tx, repo, r(repo, name)).unwrap().data;
            let blob = |repo, expr: &str| match git.expression(tx, repo, expr).unwrap().data {
                ObjectData::Blob(b) => String::from_utf8(b.to_vec()).unwrap(),
                _ => panic!("{expr} is not a blob"),
            };
            let pins = tx.table::<Repos>().by_name(ORG_ID, "pins").unwrap().id;
            let main = r(PUBLIC_REPO_ID, "refs/heads/main");
            // Then
            assert_eq!(r(PUBLIC_REPO_ID, "refs/heads/develop"), main);
            let ObjectData::Commit(feature) = object(PUBLIC_REPO_ID, "refs/heads/feature") else {
                panic!("feature is not a commit");
            };
            assert_eq!(feature.parents, [main]);
            assert_eq!(feature.message.trim_end(), "add the feature");
            let ObjectData::Tag(tag) = object(PUBLIC_REPO_ID, "refs/tags/v1.0.0") else {
                panic!("v1.0.0 is not an annotated tag");
            };
            assert_eq!(tag.target, main);
            assert_eq!(blob(PUBLIC_REPO_ID, "main:README.md"), "# public\n");
            assert_eq!(blob(PUBLIC_REPO_ID, "feature:README.md"), "# public\n");
            assert_eq!(blob(PUBLIC_REPO_ID, "feature:src/feature.txt"), "feature\n");
            assert_eq!(blob(PRIVATE_REPO_ID, "main:README.md"), "# private\n");
            assert_eq!(blob(pins, "main:pin.txt"), main.to_string());
            Ok(())
        })
        .unwrap();
}
