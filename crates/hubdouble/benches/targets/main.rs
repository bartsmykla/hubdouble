mod gate;
mod row;

use std::convert::Infallible;
use std::fmt::Write as _;
use std::io::Write as _;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use base64::Engine as _;
use bytes::Bytes;
use http::{Method, Request, Response, StatusCode, header};
use http_body_util::{BodyExt, Full};
use hubdouble_model::{Config, Event, SystemClock};
use hubdouble_server::{Registries, Server};
use hubdouble_testkit::fixture;
use hyper::client::conn::http1::SendRequest;
use hyper_util::rt::TokioIo;
use mimalloc::MiMalloc;
use row::{CALIBRATION, Goal, Row, number};
use serde_json::{Map, Value, json};
use tempfile::TempDir;
use tokio::net::{TcpListener, TcpStream};
use tokio::runtime::Runtime;

#[global_allocator]
static ALLOCATOR: MiMalloc = MiMalloc;

const ROOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");
const TMP: &str = env!("CARGO_TARGET_TMPDIR");
const DOCS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/inventory/graphql");
const PLACEHOLDER_OID: &str = "0123456789abcdef0123456789abcdef01234567";
const USER_TOKEN: &str = "ghp_0123456789abcdef0123456789abcdef0123";
const WARMUP: usize = 200;
const SAMPLES: usize = 2_000;
const DOC_ROUNDS: usize = 5;
const MIXED_CONNECTIONS: usize = 16;
const MIXED_PER_CONNECTION: usize = 2_500;
const SLOW_WRITERS: usize = 16;
const SLOW_WRITES_PER_WRITER: usize = 1_500;
const SLOW_DELAY: Duration = Duration::from_secs(2);
const ENQUEUE_EVENTS: usize = 5_000;
const ENQUEUE_ROUNDS: usize = 5;
const START_RUNS: usize = 5;
const IDLE_RSS_MB: f64 = 40.0;
const MEMORY_LIMIT_MIB: f64 = 128.0;
const MIB: f64 = 1024.0 * 1024.0;
const RSS_GROWTH_MIB: f64 = MEMORY_LIMIT_MIB - IDLE_RSS_MB * 1e6 / MIB;
const FAST_EVENTS: usize = 50_000;
const FAST_BATCH: usize = 1_000;
const TEAMS: usize = 100;
const CALIBRATION_ROUNDS: usize = 5;
const CALIBRATION_ITERATIONS: usize = 200;
const PUSH_PULLS: usize = 100;
const PUSH_WARMUP: usize = 20;
const PUSH_SAMPLES: usize = 500;

fn micros(d: Duration) -> f64 {
    d.as_secs_f64() * 1e6
}

#[expect(
    clippy::cast_precision_loss,
    reason = "counts in a benchmark report stay far below 2^52"
)]
fn float(n: usize) -> f64 {
    n as f64
}

fn percentile(sorted: &[Duration], p: f64) -> Duration {
    if sorted.is_empty() {
        return Duration::ZERO;
    }
    let rank = (p * float(sorted.len())).ceil();
    let index = format!("{rank:.0}").parse::<usize>().unwrap_or(1).max(1) - 1;
    sorted[index.min(sorted.len() - 1)]
}

fn fail(what: &str, e: impl std::fmt::Display) -> ! {
    panic!("{what}: {e}")
}

fn runtime(workers: usize) -> Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(workers)
        .enable_all()
        .build()
        .unwrap_or_else(|e| fail("build a runtime", e))
}

struct Client {
    send: SendRequest<Full<Bytes>>,
    host: String,
    auth: String,
}

struct Answer {
    status: StatusCode,
    body: Bytes,
}

impl Answer {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }
}

impl Client {
    async fn connect(addr: SocketAddr, token: &str) -> Client {
        let stream = TcpStream::connect(addr)
            .await
            .unwrap_or_else(|e| fail("connect", e));
        let _ = stream.set_nodelay(true);
        let (send, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
            .await
            .unwrap_or_else(|e| fail("handshake", e));
        tokio::spawn(async move {
            let _ = conn.await;
        });
        Client {
            send,
            host: addr.to_string(),
            auth: format!("Bearer {token}"),
        }
    }

    async fn call(&mut self, method: Method, path: &str, body: Option<Bytes>) -> Answer {
        let mut request = Request::builder()
            .method(method)
            .uri(path)
            .header(header::HOST, &self.host)
            .header(header::ACCEPT, "application/vnd.github.v3+json")
            .header(header::USER_AGENT, "go-github/v93.0.0")
            .header("X-Github-Api-Version", "2022-11-28")
            .header(header::AUTHORIZATION, &self.auth);
        if body.is_some() {
            request = request.header(header::CONTENT_TYPE, "application/json");
        }
        let request = request
            .body(Full::new(body.unwrap_or_default()))
            .unwrap_or_else(|e| fail("build a request", e));
        self.send
            .ready()
            .await
            .unwrap_or_else(|e| fail("wait for the connection", e));
        let response = self
            .send
            .send_request(request)
            .await
            .unwrap_or_else(|e| fail(path, e));
        let status = response.status();
        let body = response
            .into_body()
            .collect()
            .await
            .unwrap_or_else(|e| fail(path, e))
            .to_bytes();
        Answer { status, body }
    }

    async fn ok(&mut self, method: Method, path: &str, body: Option<Value>) -> Value {
        let bytes = body.map(|b| Bytes::from(b.to_string()));
        let answer = self.call(method.clone(), path, bytes).await;
        assert!(
            answer.status.is_success(),
            "{method} {path}: {} {}",
            answer.status,
            String::from_utf8_lossy(&answer.body)
        );
        answer.json()
    }

    async fn graphql(&mut self, query: &str, variables: &Value) -> Value {
        let body = self
            .ok(
                Method::POST,
                "/graphql",
                Some(json!({"query": query, "variables": variables})),
            )
            .await;
        assert!(body.get("errors").is_none(), "{query}: {body}");
        body["data"].clone()
    }
}

#[derive(Default)]
struct Opened {
    last_use: Option<Instant>,
    in_stretch: usize,
    most: usize,
}

impl Opened {
    fn connection(&mut self) {
        let now = Instant::now();
        if self
            .last_use
            .is_none_or(|t| now - t >= hubdouble_hooks::IDLE_REUSE)
        {
            self.in_stretch = 0;
        }
        self.in_stretch += 1;
        self.most = self.most.max(self.in_stretch);
        self.last_use = Some(now);
    }

    fn answered(&mut self) {
        self.last_use = Some(Instant::now());
    }
}

fn opened(o: &Mutex<Opened>) -> MutexGuard<'_, Opened> {
    o.lock().unwrap_or_else(PoisonError::into_inner)
}

struct Receiver {
    runtime: Runtime,
    url: String,
    payload_bytes: Arc<AtomicUsize>,
    accepted: Arc<Mutex<Opened>>,
}

impl Receiver {
    fn start(delay: Duration) -> Receiver {
        let runtime = runtime(1);
        let payload_bytes = Arc::new(AtomicUsize::new(0));
        let accepted = Arc::new(Mutex::new(Opened::default()));
        let listener = runtime
            .block_on(TcpListener::bind("127.0.0.1:0"))
            .unwrap_or_else(|e| fail("bind the receiver", e));
        let addr = listener
            .local_addr()
            .unwrap_or_else(|e| fail("read the receiver address", e));
        let size = payload_bytes.clone();
        let count = accepted.clone();
        runtime.spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                opened(&count).connection();
                let (size, count) = (size.clone(), count.clone());
                tokio::spawn(async move {
                    let service =
                        hyper::service::service_fn(move |req: Request<hyper::body::Incoming>| {
                            let (size, count) = (size.clone(), count.clone());
                            async move {
                                let body = req.into_body().collect().await.map_or_else(
                                    |_| Bytes::new(),
                                    http_body_util::Collected::to_bytes,
                                );
                                size.store(body.len(), Ordering::Relaxed);
                                if !delay.is_zero() {
                                    tokio::time::sleep(delay).await;
                                }
                                opened(&count).answered();
                                Ok::<_, Infallible>(Response::new(Full::new(Bytes::new())))
                            }
                        });
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), service)
                        .await;
                });
            }
        });
        Receiver {
            runtime,
            url: format!("http://{addr}/hook"),
            payload_bytes,
            accepted,
        }
    }

    fn connections_row(&self, load: &str) -> Row {
        let pool = hubdouble_hooks::Options::default().workers;
        Row::new(
            format!("{load}: webhook connections opened"),
            Some(float(opened(&self.accepted).most)),
            "conns",
            Goal::Below(float(pool) + 0.5),
        )
        .note(format!(
            "delivery attempts share a pool of {pool} keep-alive connections; most opened \
             between two pauses of {} s, after which the pool drops idle connections",
            hubdouble_hooks::IDLE_REUSE.as_secs()
        ))
    }
}

struct Running {
    runtime: Runtime,
    addr: SocketAddr,
    server: Server,
}

fn start_server(entries: &[Value], webhook: &str, dir: &Path, name: &str) -> Running {
    let runtime = runtime(1);
    let seed = dir.join(name);
    std::fs::write(
        &seed,
        serde_json::to_vec(entries).unwrap_or_else(|e| fail("write the seed", e)),
    )
    .unwrap_or_else(|e| fail("write the seed", e));
    let webhook = webhook.to_owned();
    let (addr, server) = runtime.block_on(async move {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap_or_else(|e| fail("bind the server", e));
        let addr = listener
            .local_addr()
            .unwrap_or_else(|e| fail("read the server address", e));
        let base = format!("http://{addr}");
        let config = Config {
            addr,
            base_url: base.clone(),
            web_url: base.clone(),
            oidc_issuer: format!("{base}/_actions"),
            seed: Some(seed),
            workers: Some(1),
            ..Config::default()
        };
        let env = |name: &str| (name == fixture::WEBHOOK_URL_ENV).then(|| webhook.clone());
        let server = Server::start(
            config,
            Arc::new(SystemClock),
            &env,
            Registries::new(hubdouble_api::register_all),
        )
        .unwrap_or_else(|e| fail("start the server", e));
        let serving = server.clone();
        tokio::spawn(async move {
            serving.serve(listener, std::future::pending()).await;
        });
        (addr, server)
    });
    Running {
        runtime,
        addr,
        server,
    }
}

fn lines(word: &str, n: usize) -> String {
    (1..=n).fold(String::new(), |mut s, i| {
        let _ = writeln!(s, "{word} line {i}");
        s
    })
}

fn bench_seed() -> Vec<Value> {
    let mut entries = fixture::seed();
    for entry in &mut entries {
        if entry["kind"] == "repository" && entry["name"] == fixture::PUBLIC_REPO {
            entry["files"] = json!({
                "README.md": "# public\n",
                ".github/pull_request_template.md": "## Summary\n\n## Testing\n",
                "docs/guide.md": lines("guide", 40),
                "src/lib.rs": lines("lib", 120),
            });
            entry["branches"] = json!(["develop", "bench-commits", "bench-ref"]);
            entry["commits"] = json!([
                {"message": "release 5.0.0", "files": {"VERSION": "5.0.0\n"},
                 "tags": [{"name": "agent/v5.0.0", "message": "v5.0.0"}]},
                {"message": "release 5.1.0", "files": {"VERSION": "5.1.0\n"},
                 "tags": [{"name": "agent/v5.1.0", "message": "v5.1.0"}]},
                {"branch": "feature", "message": "add the feature",
                 "files": {"src/feature.txt": lines("feature", 60),
                           "src/lib.rs": lines("lib changed", 120)}},
                {"branch": "feature", "message": "extend the feature",
                 "files": {"docs/guide.md": lines("guide changed", 40)}},
                {"branch": "prompts/proposal-1", "message": "propose",
                 "files": {"prompts/proposal-1.md": lines("proposal", 10)}},
                {"branch": "draft-work", "message": "draft",
                 "files": {"draft.txt": lines("draft", 10)}},
            ]);
        }
    }
    entries.extend([
        json!({"kind": "pull_request", "repository": "acme/public", "user": fixture::BOB,
               "title": "Add the feature", "head": "feature", "base": "main",
               "body": "Adds the feature."}),
        json!({"kind": "pull_request", "repository": "acme/public", "user": fixture::ALICE,
               "title": "Proposal 1", "head": "prompts/proposal-1", "base": "main",
               "body": "A proposal."}),
        json!({"kind": "pull_request", "repository": "acme/public", "user": fixture::ALICE,
               "title": "Draft work", "head": "draft-work", "base": "main", "draft": true}),
        json!({"kind": "release", "repository": "acme/public", "tag_name": "v1.0.0",
               "name": "v1.0.0", "author": fixture::ALICE}),
    ]);
    for review in 0..4 {
        let id = 900_001 + review;
        entries.push(
            json!({"kind": "review", "id": id, "repository": "acme/public",
                            "pull_number": 1, "author": fixture::ALICE, "event": "COMMENT",
                            "body": format!("Review {review}")}),
        );
        for thread in 0..5 {
            entries.push(
                json!({"kind": "review_thread", "review_id": id, "comment": {
                "body": format!("Comment {review}.{thread}"), "path": "src/feature.txt",
                "line": review * 10 + thread + 1}}),
            );
        }
    }
    for n in 0..5 {
        entries.push(json!({"kind": "check_run", "repository": "acme/public",
                            "app": fixture::APP_SLUG, "name": format!("check {n}"),
                            "head_sha": "feature", "conclusion": "success"}));
    }
    for n in 0..3 {
        entries.push(
            json!({"kind": "status", "repository": "acme/public", "sha": "feature",
                            "creator": fixture::ALICE, "state": "success",
                            "context": format!("ci/{n}")}),
        );
    }
    for n in 1..=TEAMS {
        let mut team = json!({"kind": "team", "org": fixture::ORG, "name": format!("Team {n}"),
                              "members": [{"login": fixture::BOB}]});
        if n > 1 {
            team["parent"] = json!(format!("team-{}", n / 2));
        } else {
            team["repositories"] = json!([{"repository": "public", "permission": "write"}]);
        }
        entries.push(team);
    }
    entries
}

struct World {
    issue: i64,
    mixed_issue: i64,
    issue_comment: Value,
    edit_ids: Vec<String>,
    thread: String,
    pull_node: String,
    draft_node: String,
    pending_review: String,
    repo_node: String,
    main: String,
    feature: String,
    bench_commits: String,
    tag_object: String,
}

fn text(v: &Value) -> String {
    v.as_str().unwrap_or_default().to_owned()
}

async fn create_issue(client: &mut Client, body: Value) -> i64 {
    client
        .ok(Method::POST, "/repos/acme/public/issues", Some(body))
        .await["number"]
        .as_i64()
        .unwrap_or_default()
}

async fn comment(client: &mut Client, issue: i64) -> (Vec<String>, Value) {
    let mut edit_ids = Vec::new();
    let mut issue_comment = Value::Null;
    for (number, count) in [(issue, 5), (1, 3)] {
        for n in 0..count {
            let comment = client
                .ok(
                    Method::POST,
                    &format!("/repos/acme/public/issues/{number}/comments"),
                    Some(json!({"body": format!("Comment {n}")})),
                )
                .await;
            edit_ids.push(text(&comment["node_id"]));
            issue_comment = comment;
        }
    }
    let id = issue_comment["id"].as_i64().unwrap_or_default();
    let edited = client
        .ok(
            Method::PATCH,
            &format!("/repos/acme/public/issues/comments/{id}"),
            Some(json!({"body": "Comment edited"})),
        )
        .await;
    (edit_ids, edited)
}

async fn reply(client: &mut Client) -> String {
    let review_comments = client
        .ok(
            Method::GET,
            "/repos/acme/public/pulls/1/comments?per_page=100",
            None,
        )
        .await;
    let review_comments = review_comments.as_array().cloned().unwrap_or_default();
    for comment in review_comments.iter().take(10) {
        for n in 0..2 {
            client
                .ok(
                    Method::POST,
                    "/repos/acme/public/pulls/1/comments",
                    Some(json!({"body": format!("Reply {n}"), "in_reply_to": comment["id"]})),
                )
                .await;
        }
    }
    review_comments
        .first()
        .map_or_else(String::new, |c| text(&c["node_id"]))
}

async fn prepare(client: &mut Client) -> World {
    let issue = create_issue(
        client,
        json!({"title": "Implement the bench", "body": "Details of the bench."}),
    )
    .await;
    let mixed_issue = create_issue(client, json!({"title": "Mixed load"})).await;
    let (mut edit_ids, issue_comment) = comment(client, issue).await;
    edit_ids.push(reply(client).await);
    let pending_review = text(
        &client
            .ok(
                Method::POST,
                "/repos/acme/public/pulls/2/reviews",
                Some(json!({"body": ""})),
            )
            .await["node_id"],
    );
    let threads = client
        .graphql(
            "query { repository(owner: \"acme\", name: \"public\") { pullRequest(number: 1) \
             { id reviewThreads(first: 1) { nodes { id } } } } }",
            &json!({}),
        )
        .await;
    let pull = &threads["repository"]["pullRequest"];
    let draft = client
        .ok(Method::GET, "/repos/acme/public/pulls/3", None)
        .await;
    let repo = client.ok(Method::GET, "/repos/acme/public", None).await;
    let mut oids = Vec::new();
    for name in [
        "heads/main",
        "heads/feature",
        "heads/bench-commits",
        "tags/agent/v5.0.0",
    ] {
        let path = format!("/repos/acme/public/git/ref/{name}");
        oids.push(text(
            &client.ok(Method::GET, &path, None).await["object"]["sha"],
        ));
    }
    let [main, feature, bench_commits, tag_object] =
        <[String; 4]>::try_from(oids).unwrap_or_else(|_| fail("read the refs", "count"));
    World {
        issue,
        mixed_issue,
        issue_comment,
        edit_ids,
        thread: text(&pull["reviewThreads"]["nodes"][0]["id"]),
        pull_node: text(&pull["id"]),
        draft_node: text(&draft["node_id"]),
        pending_review,
        repo_node: text(&repo["node_id"]),
        main,
        feature,
        bench_commits,
        tag_object,
    }
}

fn rest_routes(w: &World) -> Vec<String> {
    let issue = w.issue;
    let comment = w.issue_comment["id"].as_i64().unwrap_or_default();
    [
        "/repos/acme/public",
        "/repos/acme/public/pulls?state=open&per_page=100",
        "/repos/acme/public/pulls/1",
        "/repos/acme/public/pulls/1/files?per_page=100",
        "/repos/acme/public/pulls/1/commits?per_page=100",
        "/repos/acme/public/pulls/1/reviews?per_page=100",
        "/repos/acme/public/pulls/1/comments?per_page=100",
        "/repos/acme/public/issues/{issue}",
        "/repos/acme/public/issues/{issue}/comments?per_page=100",
        "/repos/acme/public/issues/comments/{comment}",
        "/repos/acme/public/commits/main",
        "/repos/acme/public/commits?sha=feature&per_page=100",
        "/repos/acme/public/compare/main...feature",
        "/repos/acme/public/contents/README.md?ref=main",
        "/repos/acme/public/git/ref/heads/main",
        "/repos/acme/public/git/matching-refs/tags/agent/",
        "/repos/acme/public/branches/main",
        "/repos/acme/public/commits/feature/check-runs",
        "/repos/acme/public/commits/feature/status",
        "/repos/acme/public/releases?per_page=100",
        "/repos/acme/private/collaborators/carol/permission",
        "/orgs/acme/teams?per_page=100",
        "/orgs/acme/members/bob",
        "/users/alice",
        "/user",
    ]
    .iter()
    .map(|p| {
        p.replace("{issue}", &issue.to_string())
            .replace("{comment}", &comment.to_string())
    })
    .collect()
}

enum Vars {
    Fixed(Value),
    CreateCommit {
        head: String,
    },
    UpdateRefs {
        name: &'static str,
        repo: String,
        a: String,
        b: String,
        current: String,
    },
}

struct Doc {
    name: String,
    query: String,
    vars: Option<Vars>,
}

fn documents(w: &World) -> Vec<Doc> {
    let mut names: Vec<String> = std::fs::read_dir(DOCS)
        .unwrap_or_else(|e| fail("read the inventory documents", e))
        .filter_map(Result::ok)
        .filter_map(|e| {
            e.file_name()
                .to_str()
                .and_then(|n| n.strip_suffix(".graphql"))
                .map(str::to_owned)
        })
        .collect();
    names.sort();
    names
        .into_iter()
        .map(|name| {
            let mut query = std::fs::read_to_string(format!("{DOCS}/{name}.graphql"))
                .unwrap_or_else(|e| fail(&name, e));
            let public = |extra: Value| {
                let mut v = json!({"owner": "acme", "name": "public"});
                if let (Some(v), Value::Object(e)) = (v.as_object_mut(), extra) {
                    v.extend(e);
                }
                Vars::Fixed(v)
            };
            let pull = || json!({"owner": "acme", "repo": "public", "number": 1, "cursor": null});
            let vars = match name.as_str() {
                "access" => Some(Vars::Fixed(
                    json!({"owner": "acme", "login": fixture::CAROL,
                                                     "n0": "public", "n1": "private"}),
                )),
                "contextThreads" => Some(public(json!({"number": 1, "after": null,
                                                       "first": 50, "recent": 5}))),
                "createCommit" => Some(Vars::CreateCommit {
                    head: w.bench_commits.clone(),
                }),
                "defaultHead" | "tags" => Some(public(json!({}))),
                "fileThread" => Some(Vars::Fixed(json!({"review": w.pending_review,
                                                        "path": "prompts/proposal-1.md",
                                                        "body": "A file comment"}))),
                "filesAt" => {
                    query = query.replace(PLACEHOLDER_OID, &w.main);
                    Some(public(json!({})))
                }
                "peelTags" => {
                    query = query.replace(PLACEHOLDER_OID, &w.tag_object);
                    Some(public(json!({})))
                }
                "head" => Some(public(json!({"ref": "refs/heads/feature"}))),
                "holdsFiles" => Some(public(json!({"e0": "HEAD:.github", "e1": "feature:src"}))),
                "implementIssue" => Some(public(json!({"number": w.issue}))),
                "implementPull" | "threadOpenings" | "threads" => {
                    Some(public(json!({"number": 1, "after": null})))
                }
                "implementThreadComments" => {
                    Some(Vars::Fixed(json!({"thread": w.thread, "after": null})))
                }
                "lastEdits" => Some(Vars::Fixed(json!({"ids": w.edit_ids}))),
                "originalCommentEdit" => {
                    Some(Vars::Fixed(json!({"id": w.issue_comment["node_id"]})))
                }
                "planThreads" | "reviewBodies" => Some(Vars::Fixed(pull())),
                "proposalOpen" => Some(public(json!({"query": "proposal-1"}))),
                "proposals" => Some(public(json!({
                    "query": "repo:acme/public is:pr is:open sort:updated-desc", "first": 20}))),
                "pullOfNode" => Some(Vars::Fixed(json!({"id": w.pull_node}))),
                "ready" => Some(Vars::Fixed(json!({"id": w.draft_node}))),
                "releasesProbe" => Some(public(json!({"branch": "refs/heads/main"}))),
                "resolveThread" => Some(Vars::Fixed(json!({"thread": w.thread}))),
                "teams" => Some(Vars::Fixed(
                    json!({"org": fixture::ORG, "login": fixture::BOB, "after": null}),
                )),
                "updateRefs" => Some(Vars::UpdateRefs {
                    name: "refs/heads/bench-ref",
                    repo: w.repo_node.clone(),
                    a: w.main.clone(),
                    b: w.feature.clone(),
                    current: w.bench_commits.clone(),
                }),
                "versionFiles" => Some(public(
                    json!({"e0": "main:README.md", "e1": "feature:src/feature.txt"}),
                )),
                "versionRef" => Some(public(json!({"r0": "refs/tags/agent/v5.0.0"}))),
                "workflows" => Some(public(
                    json!({"before": "main:README.md", "after": "feature:src/feature.txt"}),
                )),
                _ => None,
            };
            Doc { name, query, vars }
        })
        .collect()
}

fn doc_variables(vars: &mut Vars, i: usize) -> Value {
    match vars {
        Vars::Fixed(v) => v.clone(),
        Vars::CreateCommit { head } => {
            let contents = base64::engine::general_purpose::STANDARD.encode(format!("{i}\n"));
            json!({"input": {
                "branch": {"repositoryNameWithOwner": "acme/public", "branchName": "bench-commits"},
                "message": {"headline": format!("bench {i}")},
                "fileChanges": {"additions": [{"path": "bench.txt", "contents": contents}]},
                "expectedHeadOid": head}})
        }
        Vars::UpdateRefs {
            name,
            repo,
            a,
            b,
            current,
        } => {
            let after = if current == a { b.clone() } else { a.clone() };
            let before = std::mem::replace(current, after.clone());
            json!({"input": {"repositoryId": repo, "refUpdates": [
                {"name": name, "beforeOid": before, "afterOid": after, "force": true}]}})
        }
    }
}

async fn measure_rest(client: &mut Client, path: &str) -> Result<Vec<Duration>, String> {
    let mut samples = Vec::with_capacity(SAMPLES);
    for i in 0..WARMUP + SAMPLES {
        let started = Instant::now();
        let answer = client.call(Method::GET, path, None).await;
        let spent = started.elapsed();
        if !answer.status.is_success() {
            return Err(format!(
                "{} {}",
                answer.status,
                String::from_utf8_lossy(&answer.body)
            ));
        }
        if i >= WARMUP {
            samples.push(spent);
        }
    }
    samples.sort();
    Ok(samples)
}

async fn measure_doc(
    client: &mut Client,
    doc: &mut Doc,
    (warmup, count): (usize, usize),
) -> Result<Vec<Duration>, String> {
    let Some(vars) = doc.vars.as_mut() else {
        return Err("no variables for this document".to_owned());
    };
    let fixed = match vars {
        Vars::Fixed(v) => Some(Bytes::from(
            json!({"query": doc.query, "variables": v}).to_string(),
        )),
        _ => None,
    };
    let mut samples = Vec::with_capacity(count);
    for i in 0..warmup + count {
        let body = fixed.clone().unwrap_or_else(|| {
            Bytes::from(
                json!({"query": doc.query, "variables": doc_variables(vars, i)}).to_string(),
            )
        });
        let started = Instant::now();
        let answer = client.call(Method::POST, "/graphql", Some(body)).await;
        let spent = started.elapsed();
        let check = i == 0 || !matches!(vars, Vars::Fixed(_));
        if check {
            let value = answer.json();
            if answer.status != StatusCode::OK || value.get("errors").is_some() {
                return Err(format!("{}: {value}", answer.status));
            }
            if let Vars::CreateCommit { head } = vars {
                value["data"]["createCommitOnBranch"]["commit"]["oid"]
                    .as_str()
                    .unwrap_or_default()
                    .clone_into(head);
            }
        }
        if i >= warmup {
            samples.push(spent);
        }
    }
    samples.sort();
    Ok(samples)
}

struct Worst {
    at: Duration,
    path: String,
}

impl Worst {
    fn keep(slot: &mut Option<Worst>, at: Duration, path: &str) {
        if slot.as_ref().is_none_or(|w| at > w.at) {
            *slot = Some(Worst {
                at,
                path: path.to_owned(),
            });
        }
    }

    fn row(slot: Option<Worst>, measure: String, goal: Goal, failed: Option<&String>) -> Row {
        if let Some(e) = failed {
            return Row::new(measure, None, "µs", goal).note(e.clone());
        }
        let (value, path) = slot.map_or((None, String::new()), |w| (Some(micros(w.at)), w.path));
        Row::new(measure, value, "µs", goal).note(path)
    }
}

fn rest_rows(runtime: &Runtime, addr: SocketAddr, routes: &[String], rows: &mut Vec<Row>) {
    let (all, p50, p99, failed) = runtime.block_on(async {
        let mut client = Client::connect(addr, fixture::ALICE_TOKEN).await;
        let (mut all, mut p50, mut p99, mut failed) = (Vec::new(), None, None, None);
        for path in routes {
            match measure_rest(&mut client, path).await {
                Ok(samples) => {
                    let (a, b) = (percentile(&samples, 0.5), percentile(&samples, 0.99));
                    eprintln!(
                        "rest {path}: p50 {:.1} µs, p99 {:.1} µs",
                        micros(a),
                        micros(b)
                    );
                    Worst::keep(&mut p50, a, path);
                    Worst::keep(&mut p99, b, path);
                    all.extend(samples);
                }
                Err(e) => failed = Some(format!("{path}: {e}")),
            }
        }
        all.sort();
        (all, p50, p99, failed)
    });
    let count = routes.len();
    for (q, label, goal) in [(0.5, "p50", 100.0), (0.99, "p99", 1000.0)] {
        let measure = format!("REST GET {label}, {count} routes");
        rows.push(match &failed {
            Some(e) => Row::new(measure, None, "µs", Goal::Below(goal))
                .scaled()
                .note(e.clone()),
            None => Row::new(
                measure,
                Some(micros(percentile(&all, q))),
                "µs",
                Goal::Below(goal),
            )
            .scaled(),
        });
    }
    rows.push(Worst::row(
        p50,
        "REST GET p50, slowest route".to_owned(),
        Goal::Report,
        failed.as_ref(),
    ));
    rows.push(Worst::row(
        p99,
        "REST GET p99, slowest route".to_owned(),
        Goal::Report,
        failed.as_ref(),
    ));
}

fn doc_rows(runtime: &Runtime, addr: SocketAddr, docs: &mut [Doc], rows: &mut Vec<Row>) {
    runtime.block_on(async {
        let mut client = Client::connect(addr, fixture::ALICE_TOKEN).await;
        let mut rounds: Vec<Result<Vec<Duration>, String>> =
            docs.iter().map(|_| Ok(Vec::new())).collect();
        for round in 0..DOC_ROUNDS {
            for (doc, p99s) in docs.iter_mut().zip(&mut rounds) {
                let Ok(done) = p99s else {
                    continue;
                };
                if round > 0 && !doc.query.trim_start().starts_with("query") {
                    continue;
                }
                match measure_doc(&mut client, doc, (WARMUP, SAMPLES)).await {
                    Ok(samples) => {
                        eprintln!(
                            "graphql {}: p50 {:.1} µs, p99 {:.1} µs",
                            doc.name,
                            micros(percentile(&samples, 0.5)),
                            micros(percentile(&samples, 0.99))
                        );
                        done.push(percentile(&samples, 0.99));
                    }
                    Err(e) => *p99s = Err(e),
                }
            }
        }
        for (doc, p99s) in docs.iter().zip(rounds) {
            let (value, note) = match p99s {
                Ok(mut p99s) => {
                    p99s.sort();
                    let note = if p99s.len() > 1 {
                        format!("median of {} rounds", p99s.len())
                    } else {
                        String::new()
                    };
                    (Some(micros(percentile(&p99s, 0.5))), note)
                }
                Err(e) => (None, e),
            };
            rows.push(
                Row::new(
                    format!("GraphQL {} p99", doc.name),
                    value.map(|v| v / 1000.0),
                    "ms",
                    Goal::Below(3.0),
                )
                .note(note),
            );
        }
    });
}

fn calibration_document() -> Value {
    let items: Vec<Value> = (0..60)
        .map(|i| {
            json!({"id": i, "node_id": format!("IC_{i:020}"), "body": lines("calibration", 2),
                   "user": {"login": "calibration", "id": i, "type": "User"},
                   "created_at": "2026-01-01T00:00:00Z", "reactions": {"total_count": 0}})
        })
        .collect();
    json!({"action": "created", "items": items})
}

fn calibrate() -> f64 {
    let document = calibration_document();
    let mut times: Vec<Duration> = (0..CALIBRATION_ROUNDS)
        .map(|_| {
            let started = Instant::now();
            for _ in 0..CALIBRATION_ITERATIONS {
                let bytes = serde_json::to_vec(std::hint::black_box(&document))
                    .unwrap_or_else(|e| fail("encode the calibration document", e));
                let parsed: Value = serde_json::from_slice(&bytes)
                    .unwrap_or_else(|e| fail("decode the calibration document", e));
                std::hint::black_box(parsed);
            }
            started.elapsed()
        })
        .collect();
    times.sort();
    micros(percentile(&times, 0.5)) / float(CALIBRATION_ITERATIONS)
}

fn calibration_row(before: f64, after: f64) -> Row {
    let size = serde_json::to_vec(&calibration_document()).map_or(0, |b| b.len());
    Row::new(CALIBRATION, Some(before.min(after)), "µs", Goal::Report).note(format!(
        "encode and decode a {size} B document, median of {CALIBRATION_ROUNDS} rounds, the faster \
         of the runs before and after the rows"
    ))
}

fn in_process(dir: &Path, rows: &mut Vec<Row>) {
    let receiver = Receiver::start(Duration::ZERO);
    let running = start_server(&bench_seed(), &receiver.url, dir, "bench-seed.json");
    let client_runtime = runtime(4);
    let addr = running.addr;
    let (world, routes) = client_runtime.block_on(async {
        let mut client = Client::connect(addr, fixture::ALICE_TOKEN).await;
        let world = prepare(&mut client).await;
        let routes = rest_routes(&world);
        (world, routes)
    });

    rest_rows(&client_runtime, addr, &routes, rows);
    let mut docs = documents(&world);
    doc_rows(&client_runtime, addr, &mut docs, rows);
    let read_docs: Vec<Bytes> = docs
        .iter()
        .filter(|d| d.query.trim_start().starts_with("query"))
        .filter_map(|d| match &d.vars {
            Some(Vars::Fixed(v)) => Some(Bytes::from(
                json!({"query": d.query, "variables": v}).to_string(),
            )),
            _ => None,
        })
        .collect();
    let (rps, errors) = mixed(
        &client_runtime,
        addr,
        &routes,
        &read_docs,
        world.mixed_issue,
    );
    let row = Row::new(
        "Mixed inventory load, reads and writes",
        (errors == 0).then_some(rps),
        "req/s",
        Goal::Above(5000.0),
    )
    .scaled();
    rows.push(if errors == 0 {
        row
    } else {
        row.note(format!("{errors} failed requests"))
    });

    let hooks = running.server.services().hooks.clone();
    let mut rounds: Vec<f64> = (0..ENQUEUE_ROUNDS)
        .map(|round| {
            client_runtime
                .block_on(hooks.drain())
                .unwrap_or_else(|e| fail("drain the webhooks", e));
            let per_event = enqueue(&running.server, &world);
            eprintln!("enqueue round {round}: {per_event:.2} µs per event");
            per_event
        })
        .collect();
    rounds.sort_by(f64::total_cmp);
    rows.push(
        Row::new(
            "Webhook enqueue per event",
            Some(rounds[ENQUEUE_ROUNDS / 2]),
            "µs",
            Goal::Below(20.0),
        )
        .scaled()
        .note(format!(
            "median of {ENQUEUE_ROUNDS} rounds, each after a drain"
        )),
    );
    rows.push(receiver.connections_row("In-process rows"));
    drop(receiver);
    running.runtime.shutdown_background();

    client_runtime.shutdown_background();
    for load in ["slow", "fast"] {
        memory_rows(load, rows);
    }
}

fn push_seed() -> Vec<Value> {
    let mut entries = fixture::seed();
    let mut commits: Vec<Value> = (1..=PUSH_PULLS)
        .map(|n| {
            json!({"branch": format!("pull-{n}"), "message": format!("change {n}"),
                   "files": {format!("changes/{n}.txt"): format!("change {n}\n")}})
        })
        .collect();
    commits.push(json!({"branch": "main-next", "message": "move main",
                        "files": {"next.txt": "next\n"}}));
    for entry in &mut entries {
        if entry["kind"] == "repository" && entry["name"] == fixture::PUBLIC_REPO {
            entry["commits"] = Value::Array(commits.clone());
        }
    }
    entries.extend((1..=PUSH_PULLS).map(|n| {
        json!({"kind": "pull_request", "repository": "acme/public", "user": fixture::ALICE,
               "title": format!("Change {n}"), "head": format!("pull-{n}"), "base": "main"})
    }));
    entries
}

async fn open_mergeable(client: &mut Client) -> usize {
    let pulls = client
        .ok(
            Method::GET,
            "/repos/acme/public/pulls?state=open&per_page=100",
            None,
        )
        .await;
    let mut mergeable = 0;
    for pull in pulls.as_array().map_or(&[][..], Vec::as_slice) {
        let path = format!("/repos/acme/public/pulls/{}", pull["number"]);
        if client.ok(Method::GET, &path, None).await["mergeable"] == json!(true) {
            mergeable += 1;
        }
    }
    mergeable
}

async fn first_reads(client: &mut Client, doc: &mut Doc) -> Result<Vec<Duration>, String> {
    let Some(vars) = doc.vars.as_mut() else {
        return Err("no variables for updateRefs".to_owned());
    };
    let mut samples = Vec::with_capacity(PUSH_SAMPLES);
    for i in 0..PUSH_WARMUP + PUSH_SAMPLES {
        let body = json!({"query": doc.query, "variables": doc_variables(vars, i)}).to_string();
        let pushed = client
            .call(Method::POST, "/graphql", Some(Bytes::from(body)))
            .await;
        if pushed.status != StatusCode::OK || pushed.json().get("errors").is_some() {
            return Err(format!("{}: {}", pushed.status, pushed.json()));
        }
        let path = format!("/repos/acme/public/pulls/{}", i % PUSH_PULLS + 1);
        let started = Instant::now();
        let answer = client.call(Method::GET, &path, None).await;
        let spent = started.elapsed();
        if answer.status != StatusCode::OK {
            return Err(format!("{path}: {}", answer.status));
        }
        if i >= PUSH_WARMUP {
            samples.push(spent);
        }
    }
    samples.sort();
    Ok(samples)
}

fn push_rows(dir: &Path, rows: &mut Vec<Row>) {
    let receiver = Receiver::start(Duration::ZERO);
    let running = start_server(&push_seed(), &receiver.url, dir, "push-seed.json");
    let client_runtime = runtime(4);
    let addr = running.addr;
    let measured = client_runtime.block_on(async {
        let mut client = Client::connect(addr, fixture::ALICE_TOKEN).await;
        let mut tips = Vec::new();
        for name in ["main", "main-next"] {
            let path = format!("/repos/acme/public/git/ref/heads/{name}");
            tips.push(text(
                &client.ok(Method::GET, &path, None).await["object"]["sha"],
            ));
        }
        let repo = client.ok(Method::GET, "/repos/acme/public", None).await;
        let [main, next] =
            <[String; 2]>::try_from(tips).unwrap_or_else(|_| fail("read the tips", "count"));
        let before = open_mergeable(&mut client).await;
        let mut doc = Doc {
            name: "updateRefs".to_owned(),
            query: std::fs::read_to_string(format!("{DOCS}/updateRefs.graphql"))
                .unwrap_or_else(|e| fail("read updateRefs", e)),
            vars: Some(Vars::UpdateRefs {
                name: "refs/heads/main",
                repo: text(&repo["node_id"]),
                a: main.clone(),
                b: next,
                current: main,
            }),
        };
        let samples = measure_doc(&mut client, &mut doc, (PUSH_WARMUP, PUSH_SAMPLES)).await;
        let reads = first_reads(&mut client, &mut doc).await;
        let after = open_mergeable(&mut client).await;
        match (samples, reads) {
            (Ok(_), Ok(_)) if before != PUSH_PULLS || after != PUSH_PULLS => Err(format!(
                "{before} open mergeable pull requests before and {after} after, \
                 {PUSH_PULLS} expected"
            )),
            (Ok(samples), Ok(reads)) => Ok((samples, reads)),
            (Err(e), _) | (_, Err(e)) => Err(e),
        }
    });
    let measure = format!("GraphQL updateRefs p99, main with {PUSH_PULLS} open pull requests");
    let read_measure = format!(
        "REST pull request read p99, first after a push to main with {PUSH_PULLS} open pull requests"
    );
    let (row, read_row) = match measured {
        Ok((samples, reads)) => {
            eprintln!(
                "first pull request read after a push: p50 {:.1} µs, p99 {:.1} µs",
                micros(percentile(&reads, 0.5)),
                micros(percentile(&reads, 0.99))
            );
            let read_row = Row::new(
                read_measure,
                Some(micros(percentile(&reads, 0.99))),
                "µs",
                Goal::Below(1000.0),
            )
            .scaled()
            .note(format!(
                "{PUSH_SAMPLES} pushes, each followed by one read of the next pull request"
            ));
            eprintln!(
                "graphql updateRefs on main: p50 {:.1} µs, p99 {:.1} µs",
                micros(percentile(&samples, 0.5)),
                micros(percentile(&samples, 0.99))
            );
            let row = Row::new(
                measure,
                Some(micros(percentile(&samples, 0.99)) / 1000.0),
                "ms",
                Goal::Below(3.0),
            )
            .note(format!(
                "{PUSH_SAMPLES} pushes that move main back and forth, each a mergeability check \
                 of every pull request"
            ));
            (row, read_row)
        }
        Err(e) => (
            Row::new(measure, None, "ms", Goal::Below(3.0)).note(e.clone()),
            Row::new(read_measure, None, "µs", Goal::Below(1000.0))
                .scaled()
                .note(e),
        ),
    };
    rows.push(row);
    rows.push(read_row);
    drop(receiver);
    running.runtime.shutdown_background();
    client_runtime.shutdown_background();
}

const MEMORY_ROWS: &str = "--memory-rows";

fn memory_rows(load: &str, rows: &mut Vec<Row>) {
    let measured = (|| {
        let dir = TempDir::new_in(TMP).map_err(|e| e.to_string())?;
        let file = dir.path().join("rows.json");
        let status = Command::new(std::env::current_exe().map_err(|e| e.to_string())?)
            .arg(MEMORY_ROWS)
            .arg(load)
            .arg(&file)
            .status()
            .map_err(|e| e.to_string())?;
        if !status.success() {
            return Err(format!("the {load} receiver process ended with {status}"));
        }
        let text = std::fs::read_to_string(&file).map_err(|e| e.to_string())?;
        let value: Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
        value["rows"]
            .as_array()
            .and_then(|r| r.iter().map(Row::from_json).collect::<Option<Vec<_>>>())
            .ok_or_else(|| format!("unreadable rows in {text}"))
    })();
    match measured {
        Ok(measured) => rows.extend(measured),
        Err(e) => {
            rows.push(
                Row::new(format!("{load} receiver rows"), None, "", Goal::Below(0.0)).note(e),
            );
        }
    }
}

fn memory_rows_child(load: &str, file: &Path) -> ExitCode {
    std::fs::create_dir_all(TMP).unwrap_or_else(|e| fail("make the temporary directory", e));
    let dir = TempDir::new_in(TMP).unwrap_or_else(|e| fail("make a temporary directory", e));
    let client_runtime = runtime(4);
    let mut rows = Vec::new();
    if load == "slow" {
        slow_receiver(dir.path(), &client_runtime, &mut rows);
    } else {
        fast_receiver(dir.path(), &client_runtime, &mut rows);
    }
    client_runtime.shutdown_background();
    let json = json!({"rows": rows.iter().map(Row::json).collect::<Vec<_>>()});
    match std::fs::write(file, json.to_string()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => fail("write the memory rows", e),
    }
}

fn mixed(
    runtime: &Runtime,
    addr: SocketAddr,
    routes: &[String],
    docs: &[Bytes],
    issue: i64,
) -> (f64, usize) {
    let routes: Arc<[String]> = routes.into();
    let docs: Arc<[Bytes]> = docs.into();
    let errors = Arc::new(AtomicUsize::new(0));
    let comment_path = format!("/repos/acme/public/issues/{issue}/comments");
    let issue_path = format!("/repos/acme/public/issues/{issue}");
    runtime.block_on(async {
        let mut clients = Vec::new();
        for _ in 0..MIXED_CONNECTIONS {
            clients.push(Client::connect(addr, fixture::ALICE_TOKEN).await);
        }
        let started = Instant::now();
        let tasks: Vec<_> = clients
            .into_iter()
            .enumerate()
            .map(|(c, mut client)| {
                let (routes, docs, errors) = (routes.clone(), docs.clone(), errors.clone());
                let (comment_path, issue_path) = (comment_path.clone(), issue_path.clone());
                tokio::spawn(async move {
                    let mut last_comment = None;
                    for i in 0..MIXED_PER_CONNECTION {
                        let k = c * 7 + i;
                        let answer = match k % 10 {
                            0..=5 => {
                                client
                                    .call(Method::GET, &routes[k % routes.len()], None)
                                    .await
                            }
                            6 | 7 => {
                                let body = docs[k % docs.len()].clone();
                                client.call(Method::POST, "/graphql", Some(body)).await
                            }
                            8 => {
                                let body = json!({"body": format!("mixed {c}.{i}")});
                                let a = client
                                    .call(
                                        Method::POST,
                                        &comment_path,
                                        Some(Bytes::from(body.to_string())),
                                    )
                                    .await;
                                last_comment = a.json()["id"].as_i64().or(last_comment);
                                a
                            }
                            _ => {
                                let (path, body) = if let Some(id) = last_comment {
                                    (
                                        format!("/repos/acme/public/issues/comments/{id}"),
                                        json!({"body": format!("edited {c}.{i}")}),
                                    )
                                } else {
                                    (
                                        issue_path.clone(),
                                        json!({"title": format!("Mixed load {c}.{i}")}),
                                    )
                                };
                                let body = Some(Bytes::from(body.to_string()));
                                client.call(Method::PATCH, &path, body).await
                            }
                        };
                        if !answer.status.is_success()
                            && errors.fetch_add(1, Ordering::Relaxed) == 0
                        {
                            eprintln!(
                                "mixed: {} {}",
                                answer.status,
                                String::from_utf8_lossy(&answer.body)
                            );
                        }
                    }
                })
            })
            .collect();
        for task in tasks {
            let _ = task.await;
        }
        let spent = started.elapsed();
        (
            float(MIXED_CONNECTIONS * MIXED_PER_CONNECTION) / spent.as_secs_f64(),
            errors.load(Ordering::Relaxed),
        )
    })
}

fn enqueue(server: &Server, world: &World) -> f64 {
    let mut payload = Map::new();
    payload.insert("action".to_owned(), json!("created"));
    payload.insert("comment".to_owned(), world.issue_comment.clone());
    payload.insert(
        "issue".to_owned(),
        json!({"number": world.issue, "title": "Implement the bench"}),
    );
    let events: Vec<Event> = (0..ENQUEUE_EVENTS)
        .map(|_| Event {
            name: "issue_comment",
            action: Some("created"),
            repo_id: Some(fixture::PUBLIC_REPO_ID),
            org_id: None,
            sender_id: fixture::ALICE_ID,
            app_id: None,
            installation_id: None,
            payload: payload.clone(),
        })
        .collect();
    let store = &server.services().store;
    let started = Instant::now();
    for event in events {
        store
            .update(|tx| {
                hubdouble_hooks::emit(tx, event);
                Ok(())
            })
            .unwrap_or_else(|e| fail("emit", e));
    }
    micros(started.elapsed()) / float(ENQUEUE_EVENTS)
}

fn rss_kib(pid: u32) -> Option<u64> {
    let out = Command::new("ps")
        .args(["-o", "rss=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

fn slow_receiver(dir: &Path, client_runtime: &Runtime, rows: &mut Vec<Row>) {
    let receiver = Receiver::start(SLOW_DELAY);
    let running = start_server(&bench_seed(), &receiver.url, dir, "slow-seed.json");
    let addr = running.addr;
    let issue = client_runtime.block_on(async {
        let mut client = Client::connect(addr, fixture::ALICE_TOKEN).await;
        client
            .ok(
                Method::POST,
                "/repos/acme/public/issues",
                Some(json!({"title": "Slow receiver"})),
            )
            .await["number"]
            .as_i64()
            .unwrap_or_default()
    });
    let hooks = running.server.services().hooks.clone();
    let rss_before = rss_kib(std::process::id());
    let (mut latencies, peak_waiting, peak_rss, failures) = client_runtime.block_on(async {
        let mut clients = Vec::new();
        for _ in 0..SLOW_WRITERS {
            clients.push(Client::connect(addr, fixture::ALICE_TOKEN).await);
        }
        let done = Arc::new(AtomicUsize::new(0));
        let monitor = {
            let (done, hooks) = (done.clone(), hooks.clone());
            tokio::spawn(async move {
                let (mut waiting, mut rss) = (0usize, Some(0u64));
                while done.load(Ordering::Relaxed) < SLOW_WRITERS {
                    waiting = waiting.max(hooks.stats().waiting_bytes);
                    rss = rss.zip(rss_kib(std::process::id())).map(|(a, b)| a.max(b));
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                (waiting, rss)
            })
        };
        let path = format!("/repos/acme/public/issues/{issue}/comments");
        let tasks: Vec<_> = clients
            .into_iter()
            .enumerate()
            .map(|(c, mut client)| {
                let (path, done) = (path.clone(), done.clone());
                tokio::spawn(async move {
                    let mut spent = Vec::with_capacity(SLOW_WRITES_PER_WRITER);
                    let mut failures = 0usize;
                    for i in 0..SLOW_WRITES_PER_WRITER {
                        let body =
                            Bytes::from(json!({"body": format!("slow {c}.{i}")}).to_string());
                        let t = Instant::now();
                        let answer = client.call(Method::POST, &path, Some(body)).await;
                        spent.push(t.elapsed());
                        if answer.status != StatusCode::CREATED {
                            failures += 1;
                        }
                    }
                    done.fetch_add(1, Ordering::Relaxed);
                    (spent, failures)
                })
            })
            .collect();
        let mut all = Vec::new();
        let mut failures = 0;
        for task in tasks {
            if let Ok((spent, f)) = task.await {
                all.extend(spent);
                failures += f;
            }
        }
        let (waiting, rss) = monitor.await.unwrap_or((0, None));
        (all, waiting, rss, failures)
    });
    latencies.sort();
    let run = SlowRun {
        latencies,
        failures,
        max_queued: hooks.stats().max_queued,
        waiting: float(peak_waiting),
        payload: float(receiver.payload_bytes.load(Ordering::Relaxed)),
        growth_kib: peak_rss
            .zip(rss_before)
            .map(|(peak, before)| peak.saturating_sub(before)),
    };
    run.rows(rows);
    rows.push(receiver.connections_row("Slow receiver"));
    running.runtime.shutdown_background();
    receiver.runtime.shutdown_background();
}

struct SlowRun {
    latencies: Vec<Duration>,
    failures: usize,
    max_queued: usize,
    waiting: f64,
    payload: f64,
    growth_kib: Option<u64>,
}

fn mib_of_kib(kib: u64) -> f64 {
    f64::from(u32::try_from(kib).unwrap_or(u32::MAX)) / 1024.0
}

fn rss_row(measure: &str, growth_kib: Option<u64>) -> Row {
    let row = Row::new(
        measure,
        growth_kib.map(mib_of_kib),
        "MiB",
        Goal::Below(RSS_GROWTH_MIB),
    );
    if growth_kib.is_some() {
        row.note(format!(
            "idle RSS under {IDLE_RSS_MB:.0} MB plus this growth fits the {MEMORY_LIMIT_MIB:.0} Mi limit"
        ))
    } else {
        row.note("the process RSS could not be read")
    }
}

impl SlowRun {
    fn rows(self, rows: &mut Vec<Row>) {
        let SlowRun {
            latencies,
            failures,
            max_queued,
            waiting,
            payload,
            growth_kib,
        } = self;
        let mib = MIB;
        let note = if failures == 0 {
            String::new()
        } else {
            format!("{failures} failed writes")
        };
        rows.push(
            Row::new(
                "Slow receiver: write p50",
                Some(micros(percentile(&latencies, 0.5))),
                "µs",
                Goal::Report,
            )
            .note(note),
        );
        rows.push(Row::new(
            "Slow receiver: write p99",
            Some(micros(percentile(&latencies, 0.99))),
            "µs",
            Goal::Report,
        ));
        rows.push(Row::new(
            "Slow receiver: worker queue depth, peak",
            Some(float(max_queued)),
            "events",
            Goal::Report,
        ));
        rows.push(
            Row::new(
                "Slow receiver: waiting payload, peak",
                Some(waiting / mib),
                "MiB",
                Goal::Below(float(hubdouble_hooks::QUEUE_BYTES) / mib),
            )
            .note(format!("{payload:.0} B per payload")),
        );
        rows.push(rss_row("Slow receiver: process RSS growth", growth_kib));
    }
}

fn fast_receiver(dir: &Path, client_runtime: &Runtime, rows: &mut Vec<Row>) {
    let receiver = Receiver::start(Duration::ZERO);
    let running = start_server(&bench_seed(), &receiver.url, dir, "fast-seed.json");
    let addr = running.addr;
    let services = running.server.services();
    let (store, hooks) = (services.store.clone(), services.hooks.clone());
    let drain = |hooks: &hubdouble_hooks::Handle| {
        client_runtime
            .block_on(hooks.drain())
            .unwrap_or_else(|e| fail("drain the webhooks", e));
    };
    client_runtime.block_on(async {
        let mut client = Client::connect(addr, fixture::ALICE_TOKEN).await;
        let issue = create_issue(&mut client, json!({"title": "Fast receiver"})).await;
        client
            .ok(
                Method::POST,
                &format!("/repos/acme/public/issues/{issue}/comments"),
                Some(json!({"body": lines("comment", 20)})),
            )
            .await;
    });
    drain(&hooks);
    let payload: Map<String, Value> = store
        .view(|tx| {
            Ok(tx
                .table::<hubdouble_store::Deliveries>()
                .iter()
                .rev()
                .find(|d| d.event == "issue_comment")
                .and_then(|d| serde_json::from_slice(&d.request.payload).ok()))
        })
        .ok()
        .flatten()
        .unwrap_or_else(|| fail("read an issue_comment delivery", "none was logged"));
    let size = serde_json::to_vec(&payload).map_or(0, |b| b.len());
    let rss_before = rss_kib(std::process::id());
    for _ in 0..FAST_EVENTS / FAST_BATCH {
        for _ in 0..FAST_BATCH {
            store
                .update(|tx| {
                    hubdouble_hooks::emit(
                        tx,
                        Event {
                            name: "issue_comment",
                            action: Some("created"),
                            repo_id: Some(fixture::PUBLIC_REPO_ID),
                            org_id: None,
                            sender_id: fixture::ALICE_ID,
                            app_id: None,
                            installation_id: None,
                            payload: payload.clone(),
                        },
                    );
                    Ok(())
                })
                .unwrap_or_else(|e| fail("emit", e));
        }
        drain(&hooks);
    }
    let growth_kib = rss_kib(std::process::id())
        .zip(rss_before)
        .map(|(after, before)| after.saturating_sub(before));
    let held = float(hooks.stats().log_bytes) / MIB;
    let answered = store
        .view(|tx| {
            Ok(tx
                .table::<hubdouble_store::Deliveries>()
                .iter()
                .filter(|d| d.status_code == 200)
                .count())
        })
        .unwrap_or_default();
    rows.push(
        rss_row(
            "Fast receiver: process RSS growth, full delivery log",
            growth_kib,
        )
        .note(format!(
            "{FAST_EVENTS} deliveries of {size} B, {held:.1} MiB of requests and responses kept"
        )),
    );
    rows.push(Row::new(
        "Fast receiver: deliveries in the log answered 200",
        Some(float(answered)),
        "deliveries",
        Goal::Above(float(FAST_EVENTS - 1)),
    ));
    rows.push(receiver.connections_row("Fast receiver"));
    running.runtime.shutdown_background();
    receiver.runtime.shutdown_background();
}

fn docker(args: &[&str]) -> Result<String, String> {
    let out = Command::new("docker")
        .args(args)
        .output()
        .map_err(|e| format!("docker {}: {e}", args.join(" ")))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(format!(
            "docker {}: {}",
            args.first().unwrap_or(&""),
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn docker_logs(id: &str, timestamps: bool) -> Result<String, String> {
    let mut command = Command::new("docker");
    command.arg("logs");
    if timestamps {
        command.arg("-t");
    }
    let out = command
        .arg(id)
        .output()
        .map_err(|e| format!("docker logs: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "docker logs: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    ))
}

fn rfc3339(text: &str) -> Option<(i64, f64)> {
    let (date, time) = text.trim().split_once('T')?;
    let time = time.strip_suffix('Z')?;
    let mut ymd = date.split('-').map(str::parse::<i64>);
    let (year, month, day) = (ymd.next()?.ok()?, ymd.next()?.ok()?, ymd.next()?.ok()?);
    let (hms, frac) = time.split_once('.').unwrap_or((time, "0"));
    let mut clock = hms.split(':').map(str::parse::<i64>);
    let (hour, minute, second) = (
        clock.next()?.ok()?,
        clock.next()?.ok()?,
        clock.next()?.ok()?,
    );
    let frac: f64 = format!("0.{frac}").parse().ok()?;
    Some((
        days_from_civil(year, month, day) * 86_400 + hour * 3_600 + minute * 60 + second,
        frac,
    ))
}

fn seconds_between(a: (i64, f64), b: (i64, f64)) -> Option<f64> {
    Some(f64::from(i32::try_from(b.0 - a.0).ok()?) + b.1 - a.1)
}

struct Container {
    id: String,
}

impl Drop for Container {
    fn drop(&mut self) {
        let _ = docker(&["rm", "-f", &self.id]);
    }
}

fn run_container(
    image: &str,
    seed_dir: &Path,
    seed: &str,
    env: &[(&str, &str)],
) -> Result<(Container, f64), String> {
    let mount = format!("{}:/seed:ro", seed_dir.display());
    let seed_env = format!("HUBDOUBLE_SEED=/seed/{seed}");
    let mut args = vec![
        "run",
        "-d",
        "--cpus=1",
        "-e",
        "HUBDOUBLE_WORKERS=1",
        "-e",
        &seed_env,
        "-v",
        &mount,
    ];
    let pairs: Vec<String> = env.iter().map(|(k, v)| format!("{k}={v}")).collect();
    for pair in &pairs {
        args.extend(["-e", pair.as_str()]);
    }
    args.push(image);
    let container = Container {
        id: docker(&args)?.trim().to_owned(),
    };
    let deadline = Instant::now() + Duration::from_secs(60);
    let line = loop {
        let logs = docker_logs(&container.id, true)?;
        if let Some(line) = logs.lines().find(|l| l.contains("listening on")) {
            break line.to_owned();
        }
        let running = docker(&["inspect", "-f", "{{.State.Running}}", &container.id])?;
        if running.trim() != "true" {
            let all = docker_logs(&container.id, false).unwrap_or_default();
            return Err(format!(
                "the container stopped before it was ready: {}",
                all.trim()
            ));
        }
        if Instant::now() > deadline {
            return Err("the container was not ready in 60 s".to_owned());
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    let started = docker(&["inspect", "-f", "{{.State.StartedAt}}", &container.id])?;
    let ready = line.split_whitespace().next().unwrap_or_default();
    let Some(seconds) = rfc3339(&started)
        .zip(rfc3339(ready))
        .and_then(|(a, b)| seconds_between(a, b))
    else {
        return Err(format!("unreadable timestamps {started:?} and {ready:?}"));
    };
    Ok((container, seconds * 1000.0))
}

fn container_rss_mb(container: &Container) -> Result<f64, String> {
    let out = docker(&["top", &container.id, "-o", "pid,rss"])?;
    let kib: f64 = out
        .lines()
        .filter_map(|l| ps_kib(l.split_whitespace().nth(1)?))
        .sum();
    if kib <= 0.0 {
        return Err(format!("no RSS in {out:?}"));
    }
    Ok(kib * 1024.0 / 1e6)
}

fn ps_kib(field: &str) -> Option<f64> {
    let (digits, scale) = match field.char_indices().last()? {
        (i, 'k' | 'K') => (&field[..i], 1.0),
        (i, 'm' | 'M') => (&field[..i], 1024.0),
        (i, 'g' | 'G') => (&field[..i], 1024.0 * 1024.0),
        _ => (field, 1.0),
    };
    Some(digits.parse::<f64>().ok()? * scale)
}

struct StartCase<'a> {
    measure: &'static str,
    dir: &'a Path,
    seed: &'static str,
    env: &'a [(&'a str, &'a str)],
    rss: bool,
}

fn container_rows(dir: &Path, rows: &mut Vec<Row>) {
    let (image, built) = if let Ok(image) = std::env::var("HUBDOUBLE_BENCH_IMAGE") {
        (image, false)
    } else {
        let image = "hubdouble:bench-targets".to_owned();
        if let Err(e) = docker(&["build", "-q", "-t", &image, ROOT]) {
            for measure in [
                "Image size",
                "Start to ready, fixture seed",
                "Start to ready, client seed",
                "Idle RSS, client seed",
            ] {
                rows.push(Row::new(measure, None, "", Goal::Below(0.0)).note(e.clone()));
            }
            return;
        }
        (image, true)
    };
    let size = docker(&["image", "inspect", "-f", "{{.Size}}", &image])
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
        .map(|b| f64::from(b) / 1e6);
    rows.push(Row::new("Image size", size, "MB", Goal::Below(20.0)));

    let fixture_seed = dir.join("fixture-seed.json");
    std::fs::write(
        &fixture_seed,
        serde_json::to_vec(&fixture::seed()).unwrap_or_else(|e| fail("write the seed", e)),
    )
    .unwrap_or_else(|e| fail("write the seed", e));
    let examples = format!("{ROOT}/examples");
    let client_env = [
        ("HUBDOUBLE_APP_PUBLIC_KEY", fixture::APP_PUBLIC_KEY),
        ("HUBDOUBLE_APP_WEBHOOK_URL", "http://127.0.0.1:9/webhook"),
        ("HUBDOUBLE_APP_WEBHOOK_SECRET", "bench-secret"),
        ("HUBDOUBLE_USER_TOKEN", USER_TOKEN),
    ];
    let fixture_env = [(fixture::WEBHOOK_URL_ENV, "http://127.0.0.1:9/webhook")];
    let cases = [
        StartCase {
            measure: "Start to ready, fixture seed",
            dir,
            seed: "fixture-seed.json",
            env: &fixture_env,
            rss: false,
        },
        StartCase {
            measure: "Start to ready, client seed",
            dir: Path::new(&examples),
            seed: "client.yaml",
            env: &client_env,
            rss: true,
        },
    ];
    let mut rss = Err("not measured".to_owned());
    for case in cases {
        let measure = case.measure;
        let mut times = Vec::new();
        let mut error = None;
        for run in 0..START_RUNS {
            match run_container(&image, case.dir, case.seed, case.env) {
                Ok((container, ms)) => {
                    times.push(ms);
                    if run == 0 && case.rss {
                        std::thread::sleep(Duration::from_secs(1));
                        rss = container_rss_mb(&container);
                    }
                }
                Err(e) => {
                    error = Some(e);
                    break;
                }
            }
        }
        times.sort_by(f64::total_cmp);
        let median = error.is_none().then(|| times[times.len() / 2]);
        eprintln!("{measure}: runs {times:?} ms");
        rows.push(
            Row::new(measure, median, "ms", Goal::Below(200.0))
                .scaled()
                .note(error.unwrap_or_else(|| format!("median of {START_RUNS}"))),
        );
    }
    let (value, note) = match rss {
        Ok(v) => (Some(v), String::new()),
        Err(e) => (None, e),
    };
    rows.push(
        Row::new(
            "Idle RSS, client seed",
            value,
            "MB",
            Goal::Below(IDLE_RSS_MB),
        )
        .note(note),
    );
    if built {
        let _ = docker(&["image", "rm", &image]);
    }
}

fn report(rows: &[Row]) -> String {
    let width = rows.iter().map(|r| r.measure.len()).max().unwrap_or(0);
    let mut out = format!(
        "{:width$}  {:>12}  {:>14}  result\n",
        "measure", "value", "target"
    );
    for row in rows {
        let value = row.value.map_or_else(
            || "n/a".to_owned(),
            |v| format!("{} {}", number(v), row.unit),
        );
        let verdict = match (row.goal, row.met()) {
            (Goal::Report, _) => "",
            (_, true) => "met",
            (_, false) => "MISS",
        };
        let _ = write!(
            out,
            "{:width$}  {value:>12}  {:>14}  {verdict}",
            row.measure,
            row.target()
        );
        if !row.note.is_empty() {
            let _ = write!(out, "  ({})", row.note);
        }
        out.push('\n');
    }
    out
}

fn main() -> ExitCode {
    let target = Path::new(TMP).parent().unwrap_or(Path::new(TMP));
    let out = std::env::var_os("HUBDOUBLE_BENCH_OUT")
        .map_or_else(|| target.join("bench-targets.json"), PathBuf::from);
    let args: Vec<String> = std::env::args().collect();
    if let [_, flag, load, file] = args.as_slice()
        && flag == MEMORY_ROWS
    {
        return memory_rows_child(load, Path::new(file));
    }
    if std::env::args().any(|a| a == "--gate") {
        let rounds = std::env::var_os("HUBDOUBLE_BENCH_ROUNDS").map(PathBuf::from);
        let cpu = std::env::var("HUBDOUBLE_BENCH_CPU").ok();
        return gate::run(target, rounds.as_deref(), cpu.as_deref());
    }
    std::fs::create_dir_all(TMP).unwrap_or_else(|e| fail("make the temporary directory", e));
    let dir = TempDir::new_in(TMP).unwrap_or_else(|e| fail("make a temporary directory", e));
    let mut rows = Vec::new();
    let before = calibrate();
    if std::env::var("HUBDOUBLE_BENCH_DOCKER").as_deref() == Ok("0") {
        eprintln!("bench:targets: HUBDOUBLE_BENCH_DOCKER=0, the container rows are skipped");
    } else {
        container_rows(dir.path(), &mut rows);
    }
    in_process(dir.path(), &mut rows);
    push_rows(dir.path(), &mut rows);
    rows.push(calibration_row(before, calibrate()));
    let table = report(&rows);
    let _ = std::io::stdout().lock().write_all(table.as_bytes());
    let json = json!({"rows": rows.iter().map(Row::json).collect::<Vec<_>>()});
    if let Err(e) = std::fs::write(&out, json.to_string()) {
        eprintln!("bench:targets: write {}: {e}", out.display());
        return ExitCode::FAILURE;
    }
    eprintln!("bench:targets: wrote {}", out.display());
    if rows.iter().all(Row::met) {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
