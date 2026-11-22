#![expect(clippy::unwrap_used, reason = "a broken test fixture must fail")]

use std::collections::BTreeMap;
use std::sync::Arc;

use http::StatusCode;
use hubdouble_git::{Commit, EntryMode, FileChange, ObjectData, Signature};
use hubdouble_model::Timestamp;
use hubdouble_testkit::fixture::{ALICE_ID, ALICE_TOKEN, PUBLIC_REPO_ID};
use hubdouble_testkit::{GoGithub, TestServer};
use serde_json::{Value, json};

async fn server() -> TestServer {
    TestServer::builder()
        .fixture()
        .register(hubdouble_api_gitdata::register)
        .start()
        .await
}

fn interleaved_merge(s: &TestServer) -> String {
    let services = s.server.services();
    let git = &services.git;
    let repo = PUBLIC_REPO_ID;
    services
        .store
        .view(|tx| {
            let base = git.resolve_ref(tx, repo, "main")?;
            let ObjectData::Commit(root) = git.object(tx, repo, base)?.data else {
                panic!("the main ref must point to a commit");
            };
            let write = |n: i64, parents| {
                let time = if n == 249 { 250 } else { n };
                git.write_commit(
                    tx,
                    repo,
                    &Commit {
                        tree: root.tree,
                        parents,
                        author: Signature::seed(),
                        committer: Signature {
                            time: Timestamp(Timestamp::SEED.0 + time),
                            ..Signature::seed()
                        },
                        message: n.to_string(),
                        gpg_signature: None,
                    },
                )
            };
            let mut tips = [base; 2];
            for n in 1..=125 {
                tips[1] = write(2 * n - 1, vec![tips[1]])?;
                tips[0] = write(2 * n, vec![tips[0]])?;
            }
            Ok(write(251, tips.to_vec())?.to_string())
        })
        .unwrap()
}

fn assert_compare_order(body: &Value, first: i64, last: i64) {
    assert_eq!(body["total_commits"], 251);
    let commits = body["commits"].as_array().unwrap();
    assert_eq!(commits.len(), (first..=last).count());
    for (commit, n) in commits.iter().zip(first..=last) {
        assert_eq!(commit["commit"]["message"], n.to_string());
    }
}

#[tokio::test]
async fn compare_orders_interleaved_branches_before_the_limit_and_pagination() {
    let s = server().await;
    let head = interleaved_merge(&s);
    let client = GoGithub::new(&s).with_token(ALICE_TOKEN);
    let path = format!("repos/acme/public/compare/main...{head}");

    let unpaged = client.get(&path).await;
    assert_eq!(unpaged.status, StatusCode::OK);
    assert_compare_order(&unpaged.json(), 2, 251);

    for (page, first, last) in [(1, 1, 100), (2, 101, 200), (3, 201, 251)] {
        let answer = client
            .get(&format!("{path}?per_page=100&page={page}"))
            .await;
        assert_eq!(answer.status, StatusCode::OK);
        assert_compare_order(&answer.json(), first, last);
    }
}

#[tokio::test]
async fn get_commit_pages_up_to_300_files_and_keeps_the_whole_commit_stats() {
    let files: BTreeMap<_, _> = (0..3_001)
        .map(|n| (format!("file-{n:04}"), "line\n"))
        .collect();
    let s = TestServer::builder()
        .fixture()
        .register(hubdouble_api_gitdata::register)
        .entry(json!({
            "kind": "repository",
            "owner": "acme",
            "name": "large-commit",
            "files": files,
        }))
        .start()
        .await;
    let client = GoGithub::new(&s).with_token(ALICE_TOKEN);
    let path = "repos/acme/large-commit/commits/main";
    for (query, count, first, next) in [
        ("?per_page=300", 300, "file-0000", Some(2)),
        ("", 300, "file-0000", Some(2)),
        ("?per_page=1", 1, "file-0000", Some(2)),
        ("?page=2", 300, "file-0300", Some(3)),
        ("?per_page=301", 300, "file-0000", Some(2)),
        ("?per_page=300&page=10", 300, "file-2700", None),
        ("?per_page=300&page=11", 0, "", None),
    ] {
        let answer = client.get(&format!("{path}{query}")).await;
        assert_eq!(answer.status, StatusCode::OK);
        let body = answer.json();
        let shown = body["files"].as_array().unwrap();
        assert_eq!(shown.len(), count, "{query}");
        assert_eq!(
            body["stats"],
            json!({"total": 3_001, "additions": 3_001, "deletions": 0})
        );
        if let Some(file) = shown.first() {
            assert_eq!(file["filename"], first);
        }
        let link = answer.header("link");
        if let Some(next) = next {
            assert!(
                link.contains(&format!("page={next}>; rel=\"next\"")),
                "{link}"
            );
        } else {
            assert!(!link.contains("rel=\"next\""), "{link}");
        }
    }
}

fn write(path: &str, data: impl Into<Arc<[u8]>>, mode: EntryMode) -> FileChange {
    FileChange::Write {
        path: path.to_owned(),
        data: data.into(),
        mode,
    }
}

fn commit_files(s: &TestServer, changes: &[FileChange]) {
    let services = s.server.services();
    let git = &services.git;
    let repo = PUBLIC_REPO_ID;
    services
        .store
        .update(|tx| {
            let parent = git.resolve_ref(tx, repo, "main")?;
            let signature = Signature::seed();
            git.commit(
                tx,
                repo,
                ALICE_ID,
                "main",
                Some(parent),
                changes,
                "write contents fixture",
                &signature,
                &signature,
                false,
            )?;
            Ok(())
        })
        .unwrap();
}

#[tokio::test]
async fn file_symlinks_keep_the_link_sha_and_read_the_target_bytes() {
    let s = server().await;
    let target = b"../config/real.json";
    commit_files(
        &s,
        &[
            write("config/real.json", b"{}\n".as_slice(), EntryMode::File),
            write("docs/preset.json", target.as_slice(), EntryMode::Symlink),
            write("docs/dir-link", b"../config".as_slice(), EntryMode::Symlink),
        ],
    );
    let services = s.server.services();
    let sha = services
        .store
        .view(|tx| {
            Ok(services
                .git
                .expression(tx, PUBLIC_REPO_ID, "main:docs/preset.json")?
                .id
                .to_string())
        })
        .unwrap();
    let client = GoGithub::new(&s).with_token(ALICE_TOKEN);
    let path = "repos/acme/public/contents/docs";
    let listed = client.get(path).await.json();
    let direct = client.get(&format!("{path}/preset.json")).await.json();

    assert_eq!(listed[0]["name"], "dir-link");
    assert_eq!(listed[0]["type"], "symlink");
    assert_eq!(listed[1]["name"], "preset.json");
    assert_eq!(
        (&listed[1]["type"], &listed[1]["sha"], &direct["sha"]),
        (&json!("file"), &json!(sha), &json!(sha))
    );
    assert_eq!(listed[1]["size"], target.len());
    assert_eq!(direct["type"], "file");
    assert_eq!(direct["size"], 3);
    assert_eq!(direct["content"], "e30K\n");
    assert_eq!(direct["git_url"], listed[1]["git_url"]);
}
