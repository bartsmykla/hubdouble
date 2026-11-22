#![expect(
    clippy::unwrap_used,
    reason = "tests fail immediately when an answer and its contract differ"
)]

use http::header::ACCEPT;
use http::{HeaderMap, HeaderValue, Method, StatusCode};
use hubdouble_testkit::fixture::ALICE_TOKEN;
use hubdouble_testkit::{Answer, GoGithub, TestServer};
use serde_json::json;

const LABELS: &str = "repos/acme/public/labels";

async fn repo_with_labels(total: usize) -> (TestServer, GoGithub) {
    let server = TestServer::builder()
        .fixture()
        .register(hubdouble_api_issues::register)
        .start()
        .await;
    let alice = GoGithub::new(&server).with_token(ALICE_TOKEN);
    let seeded = alice.get(&format!("{LABELS}?per_page=100")).await;
    let have = seeded.json().as_array().unwrap().len();
    for n in have..total {
        let made = alice
            .post(LABELS, &json!({"name": format!("label-{n:03}")}))
            .await;
        assert_eq!(made.status, StatusCode::CREATED);
    }
    (server, alice)
}

fn links(answer: &Answer) -> Vec<(String, String)> {
    answer
        .header("link")
        .split(", ")
        .filter(|l| !l.is_empty())
        .map(|l| {
            let (url, rel) = l.split_once("; rel=").unwrap();
            let page = url
                .trim_matches(['<', '>'])
                .rsplit_once("page=")
                .unwrap()
                .1
                .to_owned();
            (rel.trim_matches('"').to_owned(), page)
        })
        .collect()
}

fn len(answer: &Answer) -> usize {
    answer.json().as_array().unwrap().len()
}

#[tokio::test]
async fn pages_hold_30_by_default_at_most_100_and_link_prev_next_last_first() {
    // Given a repository with 130 labels
    let (_server, alice) = repo_with_labels(130).await;

    // When the labels are listed a page at a time
    let first = alice.get(LABELS).await;
    let middle = alice.get(&format!("{LABELS}?page=2")).await;
    let last = alice.get(&format!("{LABELS}?page=5")).await;
    let capped = alice.get(&format!("{LABELS}?per_page=1000")).await;
    let past_the_end = alice.get(&format!("{LABELS}?page=9")).await;

    // Then each page follows GitHub's per_page and Link rules
    assert_eq!(len(&first), 30);
    assert_eq!(
        links(&first),
        [("next".into(), "2".into()), ("last".into(), "5".into())]
    );
    assert_eq!(
        links(&middle),
        [
            ("prev".into(), "1".into()),
            ("next".into(), "3".into()),
            ("last".into(), "5".into()),
            ("first".into(), "1".into()),
        ]
    );
    assert_eq!(len(&last), 10);
    assert_eq!(
        links(&last),
        [("prev".into(), "4".into()), ("first".into(), "1".into())]
    );
    assert_eq!(len(&capped), 100);
    assert!(capped.header("link").contains("per_page=1000"));
    assert_eq!(past_the_end.status, StatusCode::OK);
    assert_eq!(len(&past_the_end), 0);
    assert!(links(&past_the_end).iter().all(|(rel, _)| rel != "next"));
}

#[tokio::test]
async fn an_unknown_media_type_still_answers_json_and_names_the_type_asked_for() {
    // Given
    let (_server, alice) = repo_with_labels(1).await;

    // When the client asks for media types the operation does not document
    let mut custom = HeaderMap::new();
    custom.insert(
        ACCEPT,
        HeaderValue::from_static("application/vnd.github.nonsense+json"),
    );
    let mut html = HeaderMap::new();
    html.insert(ACCEPT, HeaderValue::from_static("text/html"));
    let custom = alice.call_headers(Method::GET, LABELS, custom).await;
    let html = alice.call_headers(Method::GET, LABELS, html).await;

    // Then GitHub serves its JSON and reports the media type it chose
    assert_eq!(custom.status, StatusCode::OK);
    assert_eq!(len(&custom), 1);
    assert_eq!(
        custom.header("x-github-media-type"),
        "github.v3; param=nonsense; format=json"
    );
    assert_eq!(html.status, StatusCode::OK);
    assert_eq!(len(&html), 1);
    assert_eq!(html.header("x-github-media-type"), "github.v3; format=json");
}
