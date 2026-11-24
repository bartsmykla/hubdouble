use std::collections::{BTreeMap, HashMap};

use hubdouble_model::{
    Account, AccountKind, ApiError, Issue, IssueComment, IssueState, Label, Pull, PullRef,
    Reaction, ReactionContent, ReactionSubjectKind, Repo, Review, ReviewComment, ReviewState,
    StateReason, SubjectType, Timestamp, Visibility, node_id,
};
use hubdouble_store::{
    Accounts, IssueComments, Issues, Labels, Reactions, Repos, ReviewComments, Reviews, TxMut,
};
use hubdouble_testkit::{GoGithub, Graphql, TestServer, fixture};
use serde::Deserialize;
use serde_json::json;

const SAMPLE: &str = include_str!("testdata/search-language-toml.json");

const SEARCH: &str = "query($query: String!) {
  search(query: $query, type: ISSUE, first: 100) {
    issueCount
    nodes { ... on PullRequest { number } ... on Issue { number } }
  }
}";

#[derive(Deserialize)]
struct Sample {
    items: Vec<Item>,
    comments: Vec<CommentRow>,
    reviews: Vec<ReviewRow>,
    live: Vec<Live>,
}

#[derive(Deserialize)]
struct Item {
    number: i64,
    title: String,
    #[serde(default)]
    body: Option<String>,
    user: String,
    state: String,
    #[serde(default)]
    state_reason: Option<String>,
    #[serde(default)]
    labels: Vec<String>,
    #[serde(default)]
    assignees: Vec<String>,
    created_at: String,
    updated_at: String,
    #[serde(default)]
    closed_at: Option<String>,
    #[serde(default)]
    reactions: BTreeMap<String, i64>,
    #[serde(default)]
    review_comments: i64,
    #[serde(default)]
    pull: Option<PullRow>,
}

#[derive(Deserialize)]
struct PullRow {
    #[serde(default)]
    merged_at: Option<String>,
    head: String,
    base: String,
    head_sha: String,
}

#[derive(Deserialize)]
struct CommentRow {
    issue: i64,
    user: String,
    body: String,
    created_at: String,
}

#[derive(Deserialize)]
struct ReviewRow {
    pull: i64,
    user: String,
    state: String,
    submitted_at: String,
}

/// A live `GET /search/issues` answer for `repo:atom/language-toml` and the query.
/// `order` names how far the live order is fixed: `exact`, the sort key that ties may
/// reorder (`comments`, `reactions`, `interactions`), or `set` for GitHub's best match.
#[derive(Deserialize)]
struct Live {
    query: String,
    total_count: i64,
    numbers: Vec<i64>,
    order: String,
}

fn time(text: &str) -> Timestamp {
    Timestamp::parse(text).unwrap_or_else(|_| panic!("a sample time: {text}"))
}

fn account(id: i64, login: &str, kind: AccountKind) -> Account {
    Account {
        id,
        node_id: node_id::entity(
            if kind == AccountKind::Organization {
                node_id::ORGANIZATION
            } else {
                node_id::USER
            },
            id,
        ),
        login: login.into(),
        kind,
        name: None,
        email: None,
        company: None,
        blog: None,
        location: None,
        bio: None,
        twitter_username: None,
        description: None,
        site_admin: false,
        verified_email: true,
        sso_enforced: false,
        app_id: None,
        created_at: Timestamp::SEED,
        updated_at: Timestamp::SEED,
    }
}

fn repo(id: i64, owner_id: i64) -> Repo {
    Repo {
        id,
        node_id: node_id::entity(node_id::REPOSITORY, id),
        owner_id,
        name: "language-toml".into(),
        description: None,
        homepage: None,
        language: Some("CoffeeScript".into()),
        topics: Vec::new(),
        visibility: Visibility::Public,
        fork_of: None,
        default_branch: "master".into(),
        archived: true,
        disabled: false,
        is_template: false,
        has_issues: true,
        has_projects: false,
        has_wiki: false,
        has_discussions: false,
        allow_merge_commit: true,
        allow_squash_merge: true,
        allow_rebase_merge: true,
        squash_merge_commit_title: hubdouble_model::SquashMergeCommitTitle::CommitOrPrTitle,
        squash_merge_commit_message: hubdouble_model::SquashMergeCommitMessage::CommitMessages,
        allow_auto_merge: false,
        delete_branch_on_merge: false,
        collaborators: Vec::new(),
        created_at: Timestamp::SEED,
        updated_at: Timestamp::SEED,
        pushed_at: None,
    }
}

fn content(name: &str) -> ReactionContent {
    match name {
        "+1" => ReactionContent::ThumbsUp,
        "-1" => ReactionContent::ThumbsDown,
        "laugh" => ReactionContent::Laugh,
        "hooray" => ReactionContent::Hooray,
        "confused" => ReactionContent::Confused,
        "heart" => ReactionContent::Heart,
        "rocket" => ReactionContent::Rocket,
        _ => ReactionContent::Eyes,
    }
}

struct Seeder<'a, 'b> {
    tx: &'a mut TxMut<'b>,
    repo: i64,
    users: HashMap<String, i64>,
    labels: HashMap<String, i64>,
    issues: HashMap<i64, i64>,
}

impl Seeder<'_, '_> {
    fn user(&mut self, login: &str) -> Result<i64, ApiError> {
        if let Some(id) = self.users.get(login) {
            return Ok(*id);
        }
        let id = self.tx.next_id();
        self.tx
            .table_mut::<Accounts>()
            .insert(account(id, login, AccountKind::User))?;
        self.users.insert(login.into(), id);
        Ok(id)
    }

    fn label(&mut self, name: &str) -> Result<i64, ApiError> {
        if let Some(id) = self.labels.get(name) {
            return Ok(*id);
        }
        let id = self.tx.next_id();
        self.tx.table_mut::<Labels>().insert(Label {
            id,
            node_id: node_id::entity(node_id::LABEL, id),
            repo_id: self.repo,
            name: name.into(),
            color: "ededed".into(),
            description: None,
            default: false,
        })?;
        self.labels.insert(name.into(), id);
        Ok(id)
    }

    fn item(&mut self, item: &Item) -> Result<(), ApiError> {
        let id = self.tx.next_id();
        let user_id = self.user(&item.user)?;
        let label_ids = item
            .labels
            .iter()
            .map(|l| self.label(l))
            .collect::<Result<_, _>>()?;
        let assignee_ids = item
            .assignees
            .iter()
            .map(|a| self.user(a))
            .collect::<Result<_, _>>()?;
        let pull = item.pull.as_ref().map(|p| self.pull(p));
        let closed = item.state == "closed";
        self.tx.table_mut::<Issues>().insert(Issue {
            id,
            node_id: node_id::entity(node_id::ISSUE, id),
            repo_id: self.repo,
            number: item.number,
            title: item.title.clone(),
            body: item.body.clone(),
            user_id,
            state: if closed {
                IssueState::Closed
            } else {
                IssueState::Open
            },
            state_reason: match item.state_reason.as_deref() {
                Some("completed") => Some(StateReason::Completed),
                Some("not_planned") => Some(StateReason::NotPlanned),
                _ => None,
            },
            locked: true,
            active_lock_reason: None,
            label_ids,
            assignee_ids,
            closed_by: None,
            created_at: time(&item.created_at),
            updated_at: time(&item.updated_at),
            closed_at: item.closed_at.as_deref().map(time),
            last_edited_at: None,
            pull,
        })?;
        self.issues.insert(item.number, id);
        let reactor = self.user("reactor")?;
        for (name, n) in &item.reactions {
            for _ in 0..*n {
                let rid = self.tx.next_id();
                self.tx.table_mut::<Reactions>().insert(Reaction {
                    id: rid,
                    node_id: node_id::entity(node_id::REACTION, rid),
                    repo_id: self.repo,
                    subject_kind: ReactionSubjectKind::Issue,
                    subject_id: id,
                    user_id: reactor,
                    content: content(name),
                    created_at: Timestamp::SEED,
                })?;
            }
        }
        for _ in 0..item.review_comments {
            self.review_comment(id, user_id)?;
        }
        Ok(())
    }

    fn pull(&mut self, p: &PullRow) -> Pull {
        let id = self.tx.next_id();
        let side = |ref_name: &str, sha: &str| PullRef {
            repo_id: Some(self.repo),
            ref_name: ref_name.into(),
            sha: sha.into(),
        };
        Pull {
            id,
            node_id: node_id::entity(node_id::PULL_REQUEST, id),
            head: side(&p.head, &p.head_sha),
            base: side(&p.base, &"0".repeat(40)),
            draft: false,
            merged_at: p.merged_at.as_deref().map(time),
            merge_commit_sha: None,
            merged_by: None,
            mergeable: None,
            maintainer_can_modify: false,
            requested_reviewers: Vec::new(),
            requested_teams: Vec::new(),
        }
    }

    fn review_comment(&mut self, issue_id: i64, user_id: i64) -> Result<(), ApiError> {
        let id = self.tx.next_id();
        self.tx
            .table_mut::<ReviewComments>()
            .insert(ReviewComment {
                id,
                node_id: node_id::entity(node_id::REVIEW_COMMENT, id),
                repo_id: self.repo,
                issue_id,
                review_id: None,
                thread_id: id,
                in_reply_to_id: None,
                user_id,
                body: String::new(),
                path: "grammars/toml.cson".into(),
                diff_hunk: String::new(),
                commit_id: "0".repeat(40),
                original_commit_id: "0".repeat(40),
                line: None,
                original_line: None,
                start_line: None,
                original_start_line: None,
                side: None,
                start_side: None,
                position: None,
                original_position: None,
                subject_type: SubjectType::Line,
                via_app_id: None,
                created_at: Timestamp::SEED,
                updated_at: Timestamp::SEED,
                last_edited_at: None,
            })?;
        Ok(())
    }

    fn comment(&mut self, c: &CommentRow) -> Result<(), ApiError> {
        let id = self.tx.next_id();
        let user_id = self.user(&c.user)?;
        let at = time(&c.created_at);
        self.tx.table_mut::<IssueComments>().insert(IssueComment {
            id,
            node_id: node_id::entity(node_id::ISSUE_COMMENT, id),
            repo_id: self.repo,
            issue_id: self.issues[&c.issue],
            user_id,
            body: c.body.clone(),
            via_app_id: None,
            created_at: at,
            updated_at: at,
            last_edited_at: None,
        })?;
        Ok(())
    }

    fn review(&mut self, r: &ReviewRow) -> Result<(), ApiError> {
        let id = self.tx.next_id();
        let user_id = self.user(&r.user)?;
        let at = time(&r.submitted_at);
        self.tx.table_mut::<Reviews>().insert(Review {
            id,
            node_id: node_id::entity(node_id::REVIEW, id),
            repo_id: self.repo,
            issue_id: self.issues[&r.pull],
            user_id,
            body: String::new(),
            state: if r.state == "APPROVED" {
                ReviewState::Approved
            } else {
                ReviewState::Commented
            },
            commit_id: None,
            submitted_at: Some(at),
            created_at: at,
            last_edited_at: None,
        })?;
        Ok(())
    }
}

fn seed(tx: &mut TxMut<'_>, sample: &Sample) -> Result<(), ApiError> {
    let org = tx.next_id();
    tx.table_mut::<Accounts>()
        .insert(account(org, "atom", AccountKind::Organization))?;
    let repo_id = tx.next_id();
    tx.table_mut::<Repos>().insert(repo(repo_id, org))?;
    let mut seeder = Seeder {
        tx,
        repo: repo_id,
        users: HashMap::new(),
        labels: HashMap::new(),
        issues: HashMap::new(),
    };
    for item in &sample.items {
        seeder.item(item)?;
    }
    for c in &sample.comments {
        seeder.comment(c)?;
    }
    for r in &sample.reviews {
        seeder.review(r)?;
    }
    Ok(())
}

fn sort_key(sample: &Sample, order: &str, number: i64) -> i64 {
    let item = sample.items.iter().find(|i| i.number == number);
    let comments = sample.comments.iter().filter(|c| c.issue == number).count();
    let comments =
        i64::try_from(comments).unwrap_or(i64::MAX) + item.map_or(0, |i| i.review_comments);
    let reactions: i64 = item.map_or(0, |i| i.reactions.values().sum());
    match order {
        "comments" => comments,
        "reactions" => reactions,
        _ => comments + reactions,
    }
}

#[tokio::test]
async fn every_documented_qualifier_and_sort_answers_as_live_github() {
    // Given the issues and pull requests of the archived atom/language-toml, as live
    // GitHub serves them
    let sample: Sample =
        serde_json::from_str(SAMPLE).unwrap_or_else(|e| panic!("the search sample: {e}"));
    let server = TestServer::builder()
        .frame(hubdouble_api::register_all)
        .fixture()
        .start()
        .await;
    server
        .server
        .services()
        .store
        .update_quiet(|tx| seed(tx, &sample))
        .unwrap_or_else(|e| panic!("seed the search sample: {e}"));
    let api = Graphql::new(GoGithub::new(&server).with_token(fixture::ALICE_TOKEN));

    let mut differences = Vec::new();
    for live in &sample.live {
        // When the live query runs on the repository
        let query = format!("repo:atom/language-toml {}", live.query);
        let body = api.query(SEARCH, &json!({"query": query})).await.json();
        let found = &body["data"]["search"];
        let numbers: Vec<i64> = found["nodes"]
            .as_array()
            .unwrap_or_else(|| panic!("{query}: {body}"))
            .iter()
            .filter_map(|n| n["number"].as_i64())
            .collect();
        let keys = |list: &[i64]| -> Vec<i64> {
            list.iter()
                .map(|n| sort_key(&sample, &live.order, *n))
                .collect()
        };
        let mut got = numbers.clone();
        let mut want = live.numbers.clone();
        got.sort_unstable();
        want.sort_unstable();
        let same = found["issueCount"] == live.total_count
            && got == want
            && match live.order.as_str() {
                "exact" => numbers == live.numbers,
                "set" => true,
                _ => keys(&numbers) == keys(&live.numbers),
            };
        if !same {
            differences.push(format!(
                "{}: {numbers:?}, live {:?}",
                live.query, live.numbers
            ));
        }
    }

    // Then every count and order is GitHub's, up to the order of ties
    assert_eq!(differences, Vec::<String>::new());
}
