use std::sync::Arc;

use hubdouble_auth::Actor;
use hubdouble_git::ReadJobs;
use hubdouble_gql::{Args, Ctx, ErrorType, Obj, Resolved};
use hubdouble_model::{Issue, IssueState, Pull, PullRef, Repo, Timestamp, node_id};
use hubdouble_render::Urls;
use hubdouble_store::{Issues, Repos};
use hubdouble_testkit::fixture::{ALICE_ID, PUBLIC_REPO_ID};
use hubdouble_testkit::{TestServer, standins};
use serde_json::json;

fn issue(id: i64, number: i64, pull: bool) -> Issue {
    Issue {
        id,
        node_id: node_id::entity(node_id::ISSUE, id),
        repo_id: PUBLIC_REPO_ID,
        number,
        title: "t".to_owned(),
        body: None,
        user_id: ALICE_ID,
        state: IssueState::Open,
        state_reason: None,
        locked: false,
        active_lock_reason: None,
        label_ids: Vec::new(),
        assignee_ids: Vec::new(),
        closed_by: None,
        created_at: Timestamp::SEED,
        updated_at: Timestamp::SEED,
        closed_at: None,
        last_edited_at: None,
        pull: pull.then(|| Pull {
            id: id + 1,
            node_id: node_id::entity(node_id::PULL_REQUEST, id + 1),
            head: PullRef {
                repo_id: Some(PUBLIC_REPO_ID),
                ref_name: "feature".to_owned(),
                sha: "0".repeat(40),
            },
            base: PullRef {
                repo_id: Some(PUBLIC_REPO_ID),
                ref_name: "main".to_owned(),
                sha: "0".repeat(40),
            },
            draft: false,
            merged_at: None,
            merge_commit_sha: None,
            merged_by: None,
            mergeable: None,
            maintainer_can_modify: false,
            requested_reviewers: Vec::new(),
            requested_teams: Vec::new(),
        }),
    }
}

fn number(n: i64) -> Args {
    Args(serde_json::Map::from_iter([(
        "number".to_owned(),
        json!(n),
    )]))
}

fn type_name(r: Result<Resolved, hubdouble_gql::GqlError>) -> Result<&'static str, ErrorType> {
    match r {
        Ok(Resolved::Object { type_name, .. }) => Ok(type_name),
        Ok(_) => Ok("other"),
        Err(e) => Err(e.kind),
    }
}

#[tokio::test]
async fn issue_and_pull_request_resolve_only_their_own_kind_of_number() {
    // Given
    let server = TestServer::builder().fixture().start().await;
    let services = server.server.services().clone();
    services
        .store
        .update_quiet(|tx| {
            tx.table_mut::<Issues>()
                .insert(issue(9001, 1, false))
                .unwrap();
            tx.table_mut::<Issues>()
                .insert(issue(9002, 2, true))
                .unwrap();
            Ok(())
        })
        .unwrap();
    let urls = Urls::new(&services.config, "");
    let registry = hubdouble_gql::Registry::new();
    // When
    let got = services
        .store
        .view(|tx| {
            let repo: Arc<Repo> = tx.table::<Repos>().get(PUBLIC_REPO_ID).unwrap().clone();
            let ctx = Ctx {
                tx,
                git: &services.git,
                read_jobs: &ReadJobs::default(),
                actor: &Actor::Anonymous,
                clock: &*services.clock,
                urls: &urls,
                registry: &registry,
            };
            let parent = Obj {
                type_name: "Repository",
                value: repo,
            };
            Ok([
                type_name(standins::issue(&ctx, &parent, &number(1))),
                type_name(standins::issue(&ctx, &parent, &number(2))),
                type_name(standins::pull_request(&ctx, &parent, &number(2))),
                type_name(standins::pull_request(&ctx, &parent, &number(1))),
                type_name(standins::issue(&ctx, &parent, &number(3))),
            ])
        })
        .unwrap();
    // Then
    assert_eq!(
        got,
        [
            Ok("Issue"),
            Err(ErrorType::NotFound),
            Ok("PullRequest"),
            Err(ErrorType::NotFound),
            Err(ErrorType::NotFound),
        ]
    );
}
