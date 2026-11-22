#![expect(clippy::unwrap_used, reason = "a broken test fixture must fail")]

use http::StatusCode;
use hubdouble_git::{Commit, ObjectData, Signature};
use hubdouble_model::Timestamp;
use hubdouble_testkit::fixture::{ALICE_TOKEN, PUBLIC_REPO_ID};
use hubdouble_testkit::{GoGithub, TestServer};
use serde_json::Value;

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
