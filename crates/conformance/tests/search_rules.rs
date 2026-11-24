use hubdouble_model::{Reaction, ReactionContent, ReactionSubjectKind, Timestamp, node_id};
use hubdouble_store::{Issues, Reactions};
use hubdouble_testkit::fixture::{self, ALICE_ID, ALICE_TOKEN, BOB_TOKEN, CAROL_TOKEN};
use hubdouble_testkit::{Answer, GoGithub, Graphql, TestServer};
use serde_json::{Value, json};

const SEARCH: &str = "query($query: String!) {
  search(query: $query, type: ISSUE, first: 100) {
    issueCount
    nodes { ... on PullRequest { number } ... on Issue { number } }
  }
}";

async fn server() -> TestServer {
    let mut builder = TestServer::builder().frame(hubdouble_api::register_all);
    for entry in fixture::seed() {
        builder = builder.entry(entry);
    }
    builder
        .entry(json!({
            "kind": "team", "org": "acme", "name": "Secret", "privacy": "secret",
            "members": [{"login": "alice"}],
            "repositories": [{"repository": "acme/public", "permission": "read"}],
        }))
        .entry(json!({
            "kind": "team", "org": "acme", "name": "Reviewers",
            "repositories": [{"repository": "acme/public", "permission": "read"}],
        }))
        .entry(json!({
            "kind": "pull_request", "repository": "acme/public", "user": "alice",
            "title": "Ship the feature", "head": "feature", "base": "main",
        }))
        .start()
        .await
}

fn created(answer: &Answer) -> Value {
    assert!(
        answer.status.is_success(),
        "{} {}",
        answer.status,
        String::from_utf8_lossy(&answer.body)
    );
    answer.json()
}

async fn numbers(client: &GoGithub, query: &str) -> Vec<i64> {
    let body = Graphql::new(client.clone())
        .query(
            SEARCH,
            &json!({"query": format!("repo:acme/public {query}")}),
        )
        .await
        .json();
    let found = &body["data"]["search"];
    let numbers: Vec<i64> = found["nodes"]
        .as_array()
        .unwrap_or_else(|| panic!("{query}: {body}"))
        .iter()
        .filter_map(|n| n["number"].as_i64())
        .collect();
    assert_eq!(found["issueCount"], numbers.len(), "{query}");
    numbers
}

#[tokio::test]
async fn a_team_qualifier_matches_only_a_team_the_searcher_can_see() {
    let server = server().await;
    let alice = GoGithub::new(&server).with_token(ALICE_TOKEN);

    // Given
    let secret = created(
        &alice
            .post(
                "repos/acme/public/issues",
                &json!({"title": "Ask", "body": "cc @acme/secret"}),
            )
            .await,
    )["number"]
        .clone();
    let closed = created(
        &alice
            .post(
                "repos/acme/public/issues",
                &json!({"title": "Ask", "body": "cc @acme/reviewers"}),
            )
            .await,
    )["number"]
        .clone();
    created(
        &alice
            .post(
                "repos/acme/public/pulls/1/requested_reviewers",
                &json!({"team_reviewers": ["secret", "reviewers"]}),
            )
            .await,
    );

    // When
    let mut seen = Vec::new();
    for token in [ALICE_TOKEN, BOB_TOKEN, CAROL_TOKEN] {
        let client = GoGithub::new(&server).with_token(token);
        let mut row = Vec::new();
        for query in [
            "team:acme/secret",
            "team:ACME/Reviewers",
            "team-review-requested:acme/secret",
            "team-review-requested:acme/reviewers",
        ] {
            row.push(numbers(&client, query).await);
        }
        seen.push(row);
    }

    // Then
    let secret = secret.as_i64().unwrap_or_default();
    let closed = closed.as_i64().unwrap_or_default();
    assert_eq!(
        seen,
        vec![
            vec![vec![secret], vec![closed], vec![1], vec![1]],
            vec![vec![], vec![closed], vec![], vec![1]],
            vec![vec![], vec![], vec![], vec![]],
        ]
    );
}

#[tokio::test]
async fn reviews_with_a_body_count_as_comments_and_comment_reactions_do_not() {
    let server = server().await;
    let alice = GoGithub::new(&server).with_token(ALICE_TOKEN);
    let bob = GoGithub::new(&server).with_token(BOB_TOKEN);

    // Given
    let comment = created(
        &bob.post(
            "repos/acme/public/issues/1/comments",
            &json!({"body": "Nice"}),
        )
        .await,
    );
    created(
        &alice
            .post(
                &format!(
                    "repos/acme/public/issues/comments/{}/reactions",
                    comment["id"]
                ),
                &json!({"content": "heart"}),
            )
            .await,
    );
    created(
        &bob.post(
            "repos/acme/public/pulls/1/reviews",
            &json!({"event": "COMMENT", "body": "One question"}),
        )
        .await,
    );
    created(
        &bob.post(
            "repos/acme/public/pulls/1/reviews",
            &json!({"event": "APPROVE"}),
        )
        .await,
    );
    server
        .server
        .services()
        .store
        .update_quiet(|tx| {
            let issue = tx
                .table::<Issues>()
                .in_repo(fixture::PUBLIC_REPO_ID)
                .find(|i| i.number == 1)
                .map_or(0, |i| i.id);
            let id = tx.next_id();
            tx.table_mut::<Reactions>().insert(Reaction {
                id,
                node_id: node_id::entity(node_id::REACTION, id),
                repo_id: fixture::PUBLIC_REPO_ID,
                subject_kind: ReactionSubjectKind::Issue,
                subject_id: issue,
                user_id: ALICE_ID,
                content: ReactionContent::ThumbsUp,
                created_at: Timestamp::SEED,
            })?;
            Ok(())
        })
        .unwrap_or_else(|e| panic!("seed the reaction: {e}"));

    // When
    let mut seen = Vec::new();
    for query in [
        "comments:2",
        "reactions:1",
        "interactions:3",
        "interactions:>3",
    ] {
        seen.push(numbers(&alice, query).await);
    }

    // Then
    assert_eq!(seen, vec![vec![1], vec![1], vec![1], vec![]]);
}
