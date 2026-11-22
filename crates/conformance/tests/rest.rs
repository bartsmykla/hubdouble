#![expect(
    clippy::unwrap_used,
    reason = "tests fail immediately when an answer and its contract differ"
)]

use http::header::ACCEPT;
use http::{HeaderMap, HeaderValue, Method, StatusCode};
use hubdouble_testkit::fixture::{ALICE_ID, ALICE_TOKEN, ORG_ID, PRIVATE_REPO_ID, PUBLIC_REPO_ID};
use hubdouble_testkit::{Answer, GoGithub, TestServer};
use serde_json::json;

const LABELS: &str = "repos/acme/public/labels";

async fn repo_with_labels(total: usize) -> (TestServer, GoGithub) {
    let server = TestServer::builder()
        .fixture()
        .register(hubdouble_api_issues::register)
        .register(hubdouble_api_repos::register)
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

fn next_link(client: &GoGithub, answer: &Answer) -> Option<String> {
    answer
        .header("link")
        .split(", ")
        .find(|l| l.ends_with("rel=\"next\""))
        .map(|l| {
            let url = l.split_once('>').unwrap().0.trim_start_matches('<');
            url.strip_prefix(client.base()).unwrap().to_owned()
        })
}

async fn follow_every_next(client: &GoGithub, first: &str) -> (Vec<String>, Vec<String>) {
    let mut names = Vec::new();
    let mut followed = Vec::new();
    let mut answer = client.get(first).await;
    loop {
        assert_eq!(answer.status, StatusCode::OK);
        let page = answer.json();
        names.extend(
            page.as_array()
                .unwrap()
                .iter()
                .map(|item| item["name"].as_str().unwrap().to_owned()),
        );
        let Some(next) = next_link(client, &answer) else {
            break;
        };
        answer = client.get(&next).await;
        followed.push(next);
    }
    (names, followed)
}

#[tokio::test]
async fn a_client_follows_githubs_by_id_next_links_to_the_last_page() {
    // Given 65 labels on acme/public and two repositories in acme
    let (_server, alice) = repo_with_labels(65).await;

    // When the client follows each next link from the first page
    let (labels, label_pages) = follow_every_next(&alice, &format!("{LABELS}?per_page=20")).await;
    let (repos, repo_pages) = follow_every_next(&alice, "orgs/acme/repos?per_page=1").await;

    // Then each link is in api.github.com's by-ID form and the pages hold every item once
    assert_eq!(
        label_pages,
        (2..=4)
            .map(|p| format!("repositories/{PUBLIC_REPO_ID}/labels?per_page=20&page={p}"))
            .collect::<Vec<_>>()
    );
    let mut unique = labels.clone();
    unique.sort();
    unique.dedup();
    assert_eq!((labels.len(), unique.len()), (65, 65));
    assert_eq!(
        repo_pages,
        [format!("organizations/{ORG_ID}/repos?per_page=1&page=2")]
    );
    assert_eq!(repos.len(), 2);
}

#[tokio::test]
async fn a_by_id_alias_answers_as_the_named_route_with_the_same_access_checks() {
    // Given
    let (server, alice) = repo_with_labels(1).await;
    let anonymous = GoGithub::new(&server);
    let private_by_id = format!("repositories/{PRIVATE_REPO_ID}/labels");

    // When
    let named = alice.get("repos/acme/private/labels").await;
    let by_id = alice.get(&private_by_id).await;
    let hidden_named = anonymous.get("repos/acme/private/labels").await;
    let hidden_by_id = anonymous.get(&private_by_id).await;
    let public_by_id = anonymous
        .get(&format!("repositories/{PUBLIC_REPO_ID}/labels"))
        .await;
    let unknown = alice.get("repositories/999999/labels").await;
    let org_by_id = alice.get(&format!("organizations/{ORG_ID}/repos")).await;
    let user_as_org = alice.get(&format!("organizations/{ALICE_ID}/repos")).await;

    // Then
    assert_eq!(named.status, StatusCode::OK);
    assert_eq!(by_id.status, StatusCode::OK);
    assert_eq!(by_id.json(), named.json());
    assert_eq!(hidden_named.status, StatusCode::NOT_FOUND);
    assert_eq!(hidden_by_id.status, StatusCode::NOT_FOUND);
    assert_eq!(public_by_id.status, StatusCode::OK);
    assert_eq!(unknown.status, StatusCode::NOT_FOUND);
    assert_eq!(org_by_id.status, StatusCode::OK);
    assert_eq!(len(&org_by_id), 2);
    assert_eq!(user_as_org.status, StatusCode::NOT_FOUND);
}

fn rel_queries(client: &GoGithub, answer: &Answer) -> Vec<(String, String)> {
    let issues = format!("{}repositories/{PUBLIC_REPO_ID}/issues?", client.base());
    answer
        .header("link")
        .split(", ")
        .filter(|l| !l.is_empty())
        .map(|l| {
            let (url, rel) = l.split_once("; rel=").unwrap();
            let query = url.trim_matches(['<', '>']).strip_prefix(&issues).unwrap();
            (rel.trim_matches('"').to_owned(), query.to_owned())
        })
        .collect()
}

fn shape(query: &str) -> String {
    query
        .split('&')
        .map(|pair| match pair.split_once('=') {
            Some((name @ ("after" | "before"), cursor)) => {
                assert!(cursor.starts_with("Y3Vyc29yOnYyOpL"), "{cursor}");
                format!("{name}=*")
            }
            _ => pair.to_owned(),
        })
        .collect::<Vec<_>>()
        .join("&")
}

fn shapes(client: &GoGithub, answer: &Answer) -> Vec<(String, String)> {
    rel_queries(client, answer)
        .into_iter()
        .map(|(rel, q)| (rel, shape(&q)))
        .collect()
}

fn numbers(answer: &Answer) -> Vec<i64> {
    answer
        .json()
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["number"].as_i64().unwrap())
        .collect()
}

#[tokio::test]
async fn the_issue_list_pages_with_githubs_after_and_before_cursors() {
    // Given five open issues on acme/public
    let (_server, alice) = repo_with_labels(1).await;
    for n in 0..5 {
        let made = alice
            .post(
                "repos/acme/public/issues",
                &json!({"title": format!("issue {n}")}),
            )
            .await;
        assert_eq!(made.status, StatusCode::CREATED);
    }
    let every = numbers(&alice.get("repos/acme/public/issues?per_page=100").await);
    assert_eq!(every.len(), 5);

    // When the client follows each next link from the first page, as live GitHub writes it
    let first = alice.get("repos/acme/public/issues?per_page=2").await;
    let mut pages = vec![first];
    while let Some(next) = next_link(&alice, pages.last().unwrap()) {
        pages.push(alice.get(&next).await);
    }
    let by_page = alice
        .get("repos/acme/public/issues?per_page=2&page=2")
        .await;
    let back = rel_queries(&alice, &pages[2])
        .into_iter()
        .find(|(rel, _)| rel == "prev")
        .unwrap()
        .1;
    let before = alice
        .get(&format!("repositories/{PUBLIC_REPO_ID}/issues?{back}"))
        .await;
    let past_the_end = alice
        .get("repos/acme/public/issues?per_page=2&page=9")
        .await;
    let bogus = alice.get("repos/acme/public/issues?after=bogus").await;

    // Then the pages hold every issue once, in order, with live GitHub's links
    assert_eq!(pages.len(), 3);
    assert_eq!(pages.iter().flat_map(numbers).collect::<Vec<_>>(), every);
    let s = |rel: &str, q: &str| (rel.to_owned(), q.to_owned());
    assert_eq!(
        shapes(&alice, &pages[0]),
        [s("next", "per_page=2&after=*&page=2")]
    );
    assert_eq!(
        shapes(&alice, &pages[1]),
        [
            s("next", "per_page=2&after=*&page=3"),
            s("prev", "per_page=2&page=1&before=*"),
        ]
    );
    assert_eq!(
        shapes(&alice, &pages[2]),
        [s("prev", "per_page=2&page=2&before=*")]
    );
    assert_eq!(numbers(&by_page), every[2..4]);
    assert_eq!(
        shapes(&alice, &by_page),
        [
            s("next", "per_page=2&page=3&after=*"),
            s("prev", "per_page=2&page=1&before=*"),
        ]
    );
    assert_eq!(numbers(&before), every[2..4]);
    assert_eq!(past_the_end.status, StatusCode::OK);
    assert_eq!(numbers(&past_the_end), Vec::<i64>::new());
    assert_eq!(past_the_end.header("link"), "");
    assert_eq!(bogus.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(bogus.json()["errors"][0]["field"], "after");
    assert_eq!(bogus.json()["errors"][0]["code"], "invalid");
    assert_eq!(bogus.json()["errors"][0]["value"], "bogus");
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
