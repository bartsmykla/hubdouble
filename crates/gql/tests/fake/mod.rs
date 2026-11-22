use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock};

use hubdouble_auth::Actor;
use hubdouble_git::{Git, GitBuilder};
use hubdouble_gql::{
    Args, Ctx, ErrorType, GqlError, MutCtx, Obj, Registry, Request, Resolved, Schema, Target,
    connection,
};
use hubdouble_model::node_id;
use hubdouble_model::{ManualClock, Timestamp};
use hubdouble_render::Urls;
use hubdouble_store::{Store, StoreBuilder, Table};
use serde::{Serialize, Serializer};
use serde_json::Value;

pub static SCHEMA: LazyLock<Arc<Schema>> = LazyLock::new(|| match Schema::load() {
    Ok(s) => Arc::new(s),
    Err(e) => panic!("{e}"),
});

/// Every mutation's body, in commit order.
#[derive(Clone, Default)]
pub struct Notes(pub Vec<String>);

impl Table for Notes {}

/// Counts how often a repository's serde form is built.
#[derive(Clone, Default)]
pub struct Probe(pub Arc<AtomicUsize>);

impl Table for Probe {}

fn counted<S: Serializer>(c: &Arc<AtomicUsize>, s: S) -> Result<S::Ok, S::Error> {
    c.fetch_add(1, Ordering::SeqCst);
    s.serialize_unit()
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Repo {
    pub name: String,
    pub name_with_owner: String,
    #[serde(serialize_with = "counted")]
    pub probe: Arc<AtomicUsize>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Pull {
    pub number: i64,
    pub title: Option<String>,
    pub state: &'static str,
    pub mergeable: &'static str,
    pub is_draft: bool,
    pub base_ref_name: &'static str,
    pub base_ref_oid: &'static str,
    pub head_ref_oid: &'static str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Issue {
    pub number: i64,
    pub title: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Thread {
    pub id: String,
    pub is_resolved: bool,
    pub is_outdated: bool,
    pub path: String,
    pub line: Option<i64>,
    #[serde(skip)]
    pub comments: Vec<i64>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Comment {
    pub database_id: i64,
    pub body: String,
    pub last_edited_at: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Review {
    pub database_id: i64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct User {
    pub login: String,
    pub database_id: i64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Payload {
    pub client_mutation_id: String,
}

fn object<T: Send + Sync + 'static>(type_name: &'static str, value: T) -> Resolved {
    Resolved::Object {
        type_name,
        value: Arc::new(value),
    }
}

fn arg<T: serde::de::DeserializeOwned>(args: &Args, name: &str) -> Result<T, GqlError> {
    args.get(name)?
        .ok_or_else(|| GqlError::new(ErrorType::Unprocessable, name.to_owned()))
}

fn repo(cx: &Ctx<'_>, owner: &str, name: &str) -> Resolved {
    object(
        "Repository",
        Repo {
            name: name.to_owned(),
            name_with_owner: format!("{owner}/{name}"),
            probe: cx.tx.table::<Probe>().0.clone(),
        },
    )
}

fn pull(number: i64) -> Resolved {
    object(
        "PullRequest",
        Pull {
            number,
            title: (number != 2).then(|| format!("pull {number}")),
            state: "OPEN",
            mergeable: "MERGEABLE",
            is_draft: false,
            base_ref_name: "main",
            base_ref_oid: "0123456789abcdef0123456789abcdef01234567",
            head_ref_oid: "89abcdef0123456789abcdef0123456789abcdef",
        },
    )
}

fn comment(id: i64) -> Comment {
    Comment {
        database_id: id,
        body: format!("comment {id}"),
        last_edited_at: None,
    }
}

fn query_repository(cx: &Ctx<'_>, _: &Obj, args: &Args) -> Result<Resolved, GqlError> {
    let owner: String = arg(args, "owner")?;
    let name: String = arg(args, "name")?;
    if name.starts_with("hidden") {
        return cx.load(Target::Repo {
            owner: &owner,
            name: &name,
        });
    }
    Ok(repo(cx, &owner, &name))
}

fn query_search(_: &Ctx<'_>, _: &Obj, args: &Args) -> Result<Resolved, GqlError> {
    let items = [1_i64, 2, 3];
    connection(
        "SearchResultItemConnection",
        &items,
        args,
        |n| format!("c{n}"),
        |&n| {
            if n % 2 == 0 {
                object(
                    "Issue",
                    Issue {
                        number: n,
                        title: format!("issue {n}"),
                    },
                )
            } else {
                pull(n)
            }
        },
    )
}

fn repository_pull(_: &Ctx<'_>, _: &Obj, args: &Args) -> Result<Resolved, GqlError> {
    Ok(pull(arg(args, "number")?))
}

#[expect(clippy::unnecessary_wraps, reason = "a resolver returns a Result")]
fn pull_repository(cx: &Ctx<'_>, _: &Obj, _: &Args) -> Result<Resolved, GqlError> {
    Ok(repo(cx, "o", "one"))
}

fn pull_threads(_: &Ctx<'_>, _: &Obj, args: &Args) -> Result<Resolved, GqlError> {
    let threads: Vec<i64> = (1..=3).collect();
    connection(
        "PullRequestReviewThreadConnection",
        &threads,
        args,
        |t| format!("t{t}"),
        |&t| thread(t),
    )
}

fn thread(t: i64) -> Resolved {
    object(
        "PullRequestReviewThread",
        Thread {
            id: node_id::entity("PRRT", t),
            is_resolved: t == 1,
            is_outdated: false,
            path: format!("src/{t}.rs"),
            line: Some(t * 10),
            comments: (t * 100..t * 100 + 40).collect(),
        },
    )
}

fn thread_comments(_: &Ctx<'_>, parent: &Obj, args: &Args) -> Result<Resolved, GqlError> {
    let t = parent.get::<Thread>()?;
    connection(
        "PullRequestReviewCommentConnection",
        &t.comments,
        args,
        |c| format!("c{c}"),
        |&c| object("PullRequestReviewComment", comment(c)),
    )
}

fn comment_author(_: &Ctx<'_>, parent: &Obj, _: &Args) -> Result<Resolved, GqlError> {
    let c = parent.get::<Comment>()?;
    Ok(object(
        "User",
        User {
            login: format!("user{}", c.database_id % 3),
            database_id: c.database_id % 3,
        },
    ))
}

fn comment_review(_: &Ctx<'_>, parent: &Obj, _: &Args) -> Result<Resolved, GqlError> {
    let c = parent.get::<Comment>()?;
    Ok(object(
        "PullRequestReview",
        Review {
            database_id: c.database_id / 100,
        },
    ))
}

fn comment_reply_to(_: &Ctx<'_>, parent: &Obj, _: &Args) -> Result<Resolved, GqlError> {
    let c = parent.get::<Comment>()?;
    Ok(if c.database_id % 100 == 0 {
        Resolved::Null
    } else {
        object("PullRequestReviewComment", comment(c.database_id - 1))
    })
}

#[expect(clippy::unnecessary_wraps, reason = "a node loader returns a Result")]
fn load_pull(_: &Ctx<'_>, id: i64) -> Result<Resolved, GqlError> {
    Ok(if id > 0 { pull(id) } else { Resolved::Null })
}

#[expect(clippy::unnecessary_wraps, reason = "a node loader returns a Result")]
fn load_thread(_: &Ctx<'_>, id: i64) -> Result<Resolved, GqlError> {
    Ok(thread(id))
}

fn add_comment(m: &mut MutCtx<'_>, args: &Args) -> Result<Resolved, GqlError> {
    let input: Value = arg(args, "input")?;
    let body = input["body"].as_str().unwrap_or_default().to_owned();
    m.tx.table_mut::<Notes>().get_mut().0.push(body.clone());
    if body == "deny" {
        return Err(GqlError::new(ErrorType::Forbidden, "denied"));
    }
    let count = m.tx.table::<Notes>().0.len();
    Ok(object(
        "AddCommentPayload",
        Payload {
            client_mutation_id: count.to_string(),
        },
    ))
}

/// The fake resolver set over GitHub's schema.
#[must_use]
pub fn registry() -> Registry {
    let mut r = Registry::with_schema(SCHEMA.clone());
    r.object::<Repo>("Repository");
    r.object::<Pull>("PullRequest");
    r.object::<Issue>("Issue");
    r.object::<Thread>("PullRequestReviewThread");
    r.object::<Comment>("PullRequestReviewComment");
    r.object::<Review>("PullRequestReview");
    r.object::<User>("User");
    r.object::<Payload>("AddCommentPayload");
    r.field("Query", "repository", query_repository);
    r.field("Query", "search", query_search);
    r.field("Repository", "pullRequest", repository_pull);
    r.field("PullRequest", "baseRepository", pull_repository);
    r.field("PullRequest", "headRepository", pull_repository);
    r.field("PullRequest", "reviewThreads", pull_threads);
    r.field("PullRequestReviewThread", "comments", thread_comments);
    r.field("PullRequestReviewComment", "author", comment_author);
    r.field(
        "PullRequestReviewComment",
        "pullRequestReview",
        comment_review,
    );
    r.field("PullRequestReviewComment", "replyTo", comment_reply_to);
    r.node("PR", "PullRequest", load_pull);
    r.node("PRRT", "PullRequestReviewThread", load_thread);
    r.mutation("addComment", add_comment);
    r
}

pub struct World {
    pub store: Store,
    pub git: Git,
    pub registry: Registry,
    pub clock: ManualClock,
    pub urls: Urls,
}

impl World {
    #[must_use]
    pub fn new(registry: Registry) -> World {
        let mut sb = StoreBuilder::new();
        sb.register_table::<Notes>().register_table::<Probe>();
        let git = GitBuilder::new(&mut sb).build();
        World {
            store: sb.build(),
            git,
            registry,
            clock: ManualClock::new(Timestamp(1_767_225_600)),
            urls: Urls {
                api: "http://localhost:8000".to_owned(),
                web: "http://localhost:8000".to_owned(),
                base: "http://localhost:8000".to_owned(),
            },
        }
    }

    /// Runs a document as an anonymous actor and answers the response as JSON.
    #[must_use]
    pub fn run(&self, query: &str, variables: &Value) -> Value {
        let variables = variables.as_object().cloned().unwrap_or_default();
        let response = hubdouble_gql::execute(
            &self.registry,
            &self.store,
            &self.git,
            &Actor::Anonymous,
            Request {
                query,
                operation_name: None,
                variables: &variables,
                clock: &self.clock,
                urls: &self.urls,
            },
        );
        match response {
            Ok(r) => serde_json::to_value(&r).unwrap_or_default(),
            Err(e) => panic!("{e}"),
        }
    }
}
