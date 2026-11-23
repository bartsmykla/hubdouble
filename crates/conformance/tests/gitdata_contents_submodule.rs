#![expect(clippy::unwrap_used, reason = "a broken test fixture must fail")]

use http::{HeaderMap, HeaderValue, Method, StatusCode};
use hubdouble_git::{Commit, EntryMode, Signature, TreeEntry};
use hubdouble_testkit::fixture::{ALICE_ID, ALICE_TOKEN, PUBLIC_REPO_ID};
use hubdouble_testkit::{GoGithub, TestServer};
use serde_json::{Value, json};

const MODULE_ID: &str = "855827c583bc30645ba427885caa40c5b81764d2";
const MODULE_URL: &str = "https://github.com/example/dependency.git";

async fn server(module_url: impl FnOnce(&str) -> String) -> TestServer {
    let s = TestServer::builder()
        .fixture()
        .register(hubdouble_api_gitdata::register)
        .start()
        .await;
    let module_url = module_url(&s.base);
    let services = s.server.services();
    let git = &services.git;
    let repo = PUBLIC_REPO_ID;
    services
        .store
        .update(|tx| {
            let modules = format!(
                "[submodule \"dependency\"]\n\tpath = nested/module\n\turl = \"{module_url}\" # clone URL\n"
            );
            let config = git.write_blob(tx, repo, modules.as_bytes())?;
            let nested = git.write_tree(tx, repo, &[TreeEntry {
                name: "module".to_owned(),
                mode: EntryMode::Submodule,
                id: MODULE_ID.parse()?,
            }])?;
            let tree = git.write_tree(tx, repo, &[
                TreeEntry { name: ".gitmodules".to_owned(), mode: EntryMode::File, id: config },
                TreeEntry { name: "nested".to_owned(), mode: EntryMode::Tree, id: nested },
            ])?;
            let id = git.write_commit(tx, repo, &Commit {
                tree,
                parents: vec![git.resolve_ref(tx, repo, "main")?],
                author: Signature::seed(),
                committer: Signature::seed(),
                message: "add submodule".to_owned(),
                gpg_signature: None,
            })?;
            git.update_ref(tx, repo, ALICE_ID, "refs/heads/modules", None, Some(id), false)?;
            Ok(())
        })
        .unwrap();
    s
}

fn assert_module(s: &TestServer, body: &Value, kind: &str) {
    let own = format!(
        "{}/repos/acme/public/contents/nested/module?ref=modules",
        s.base
    );
    let git = format!("{}/repos/example/dependency/git/trees/{MODULE_ID}", s.base);
    let html = format!("{}/example/dependency/tree/{MODULE_ID}", s.base);
    assert_eq!(body["type"], kind);
    assert_eq!(body["name"], "module");
    assert_eq!(body["path"], "nested/module");
    assert_eq!(body["sha"], MODULE_ID);
    assert_eq!(body["size"], 0);
    assert_eq!(body["url"], own);
    assert_eq!(body["git_url"], git);
    assert_eq!(body["html_url"], html);
    assert_eq!(
        body["_links"],
        json!({"self": own, "git": git, "html": html})
    );
    assert_eq!(body.get("download_url"), Some(&Value::Null));
}

#[tokio::test]
async fn direct_contents_reads_submodule_metadata_at_the_requested_ref() {
    let s = server(|_| MODULE_URL.to_owned()).await;
    let client = GoGithub::new(&s).with_token(ALICE_TOKEN);
    let answer = client
        .get("repos/acme/public/contents/nested/module?ref=modules")
        .await;
    assert_eq!(answer.status, StatusCode::OK);
    let body = answer.json();
    assert_module(&s, &body, "submodule");
    assert_eq!(body["submodule_git_url"], MODULE_URL);
}

#[tokio::test]
async fn a_submodule_on_the_doubles_own_web_host_links_to_that_repository() {
    let s = server(|base| format!("{base}/example/dependency.git")).await;
    let client = GoGithub::new(&s).with_token(ALICE_TOKEN);
    let answer = client
        .get("repos/acme/public/contents/nested/module?ref=modules")
        .await;
    assert_eq!(answer.status, StatusCode::OK);
    assert_module(&s, &answer.json(), "submodule");
}

#[tokio::test]
async fn contents_directory_lists_submodules_by_api_version() {
    let s = server(|_| "../../example/dependency.git".to_owned()).await;
    let client = GoGithub::new(&s).with_token(ALICE_TOKEN);
    let path = "repos/acme/public/contents/nested?ref=modules";
    for (version, kind) in [("2022-11-28", "file"), ("2026-03-10", "submodule")] {
        let mut headers = HeaderMap::new();
        headers.insert("x-github-api-version", HeaderValue::from_static(version));
        let answer = client.call_headers(Method::GET, path, headers).await;
        assert_eq!(answer.status, StatusCode::OK);
        let body = answer.json();
        assert_module(&s, &body[0], kind);
        assert!(body[0].get("submodule_git_url").is_none());
    }
}
