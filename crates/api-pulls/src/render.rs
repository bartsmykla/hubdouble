use hubdouble_git::{Commit, FileDiff, Git, ObjectId};
use hubdouble_model::{ApiError, Issue, PullRef, Timestamp, node_id};
use hubdouble_render::{
    Label, PullBranch, PullRequest, Repository, SimpleUser, TeamSimple, Urls, escape,
};
use hubdouble_store::{Accounts, IssueComments, Labels, Repos, ReviewComments, Teams, Tx};
use serde::Serialize;

use crate::state;

pub(crate) fn full_name(tx: &Tx, repo_id: i64) -> Result<String, ApiError> {
    let (repo, owner) = tx
        .table::<Repos>()
        .get(repo_id)
        .and_then(|repo| {
            tx.table::<Accounts>()
                .get(repo.owner_id)
                .map(|owner| (repo, owner))
        })
        .ok_or_else(ApiError::not_found)?;
    Ok(format!("{}/{}", owner.login, repo.name))
}

pub(crate) fn user(tx: &Tx, urls: &Urls, id: i64) -> Option<SimpleUser> {
    tx.table::<Accounts>()
        .get(id)
        .map(|a| SimpleUser::new(urls, a))
}

pub(crate) fn pull(
    tx: &Tx,
    urls: &Urls,
    issue: &Issue,
    full: &str,
) -> Result<PullRequest, ApiError> {
    let p = state::pull(issue)?;
    let base = branch(tx, urls, &p.base, issue.user_id)?;
    let head = branch(tx, urls, &p.head, issue.user_id)?;
    let Some(mut answer) =
        PullRequest::new(urls, issue, full, user(tx, urls, issue.user_id), head, base)
    else {
        return Err(ApiError::not_found());
    };
    answer.labels = issue
        .label_ids
        .iter()
        .filter_map(|id| tx.table::<Labels>().get(*id))
        .map(|label| Label::new(urls, label, full))
        .collect();
    answer.assignees = issue
        .assignee_ids
        .iter()
        .filter_map(|id| user(tx, urls, *id))
        .collect();
    answer.assignee = answer.assignees.first().cloned();
    answer.requested_reviewers = p
        .requested_reviewers
        .iter()
        .filter_map(|id| user(tx, urls, *id))
        .collect();
    answer.requested_teams = p
        .requested_teams
        .iter()
        .filter_map(|id| tx.table::<Teams>().get(*id))
        .filter_map(|team| {
            tx.table::<Accounts>()
                .get(team.org_id)
                .map(|org| TeamSimple::new(urls, team, org))
        })
        .collect();
    answer.merged_by = p.merged_by.and_then(|id| user(tx, urls, id));
    answer.comments = count(tx.table::<IssueComments>().children(issue.id).count());
    answer.review_comments = count(tx.table::<ReviewComments>().children(issue.id).count());
    if let Some(repo) = tx.table::<Repos>().get(issue.repo_id) {
        answer.author_association = crate::association(tx, repo, issue.user_id);
    }
    answer.mergeable_state = crate::mergeable_state(tx, issue);
    Ok(answer)
}

fn branch(tx: &Tx, urls: &Urls, reference: &PullRef, author: i64) -> Result<PullBranch, ApiError> {
    let repo = reference.repo_id.and_then(|id| tx.table::<Repos>().get(id));
    let owner = tx
        .table::<Accounts>()
        .get(repo.map_or(author, |repo| repo.owner_id))
        .ok_or_else(ApiError::not_found)?;
    Ok(PullBranch::new(
        urls,
        reference,
        owner,
        repo.map(|r| Repository::new(urls, r, owner)),
    ))
}

pub(crate) fn detail(
    git: &Git,
    tx: &Tx,
    urls: &Urls,
    issue: &Issue,
) -> Result<PullRequest, ApiError> {
    let mut answer = pull(tx, urls, issue, &full_name(tx, issue.repo_id)?)?;
    let comparison = state::compare(git, tx, issue)?;
    answer.commits = count(comparison.commits.len());
    answer.changed_files = count(comparison.files.len());
    answer.additions = i64::try_from(comparison.files.iter().map(|f| f.additions).sum::<u64>())
        .unwrap_or(i64::MAX);
    answer.deletions = i64::try_from(comparison.files.iter().map(|f| f.deletions).sum::<u64>())
        .unwrap_or(i64::MAX);
    Ok(answer)
}

pub(crate) fn count(n: usize) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

#[derive(Serialize)]
pub(crate) struct FileResponse<'a> {
    sha: ObjectId,
    filename: &'a str,
    status: &'static str,
    additions: u64,
    deletions: u64,
    changes: u64,
    blob_url: String,
    raw_url: String,
    contents_url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    patch: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    previous_filename: Option<&'a str>,
}

pub(crate) fn file<'a>(
    urls: &Urls,
    full: &str,
    base: &str,
    head: &str,
    file: &'a FileDiff,
) -> FileResponse<'a> {
    let (sha, path) = match file.status {
        hubdouble_git::FileStatus::Removed => (base, file.path.as_str()),
        _ => (head, file.path.as_str()),
    };
    let escaped = path.split('/').map(escape).collect::<Vec<_>>().join("/");
    FileResponse {
        sha: file.blob,
        filename: &file.path,
        status: file.status.as_str(),
        additions: file.additions,
        deletions: file.deletions,
        changes: file.additions + file.deletions,
        blob_url: format!("{}/{full}/blob/{sha}/{escaped}", urls.web),
        raw_url: format!("{}/{full}/raw/{sha}/{escaped}", urls.web),
        contents_url: format!("{}/repos/{full}/contents/{escaped}?ref={sha}", urls.api),
        patch: file.patch.as_deref(),
        previous_filename: file.previous_path.as_deref(),
    }
}

#[derive(Serialize)]
pub(crate) struct CommitResponse<'a> {
    #[serde(flatten)]
    link: CommitLink,
    node_id: String,
    comments_url: String,
    author: Option<SimpleUser>,
    committer: Option<SimpleUser>,
    parents: Vec<CommitLink>,
    commit: CommitBody<'a>,
}

#[derive(Serialize)]
struct CommitLink {
    sha: ObjectId,
    url: String,
    html_url: String,
}

#[derive(Serialize)]
struct CommitBody<'a> {
    author: GitAuthor<'a>,
    committer: GitAuthor<'a>,
    message: &'a str,
    tree: TreeLink,
    url: String,
    comment_count: u64,
    verification: Verification,
}

#[derive(Serialize)]
struct TreeLink {
    sha: ObjectId,
    url: String,
}

#[derive(Serialize)]
struct GitAuthor<'a> {
    name: &'a str,
    email: &'a str,
    date: Timestamp,
}

impl<'a> From<&'a hubdouble_git::Signature> for GitAuthor<'a> {
    fn from(s: &'a hubdouble_git::Signature) -> Self {
        Self {
            name: &s.name,
            email: &s.email,
            date: s.time,
        }
    }
}

#[derive(Serialize)]
struct Verification {
    verified: bool,
    reason: &'static str,
    signature: Option<String>,
    payload: Option<String>,
    verified_at: Option<Timestamp>,
}

pub(crate) fn commit<'a>(
    git: &Git,
    tx: &Tx,
    urls: &Urls,
    full: &str,
    repo_id: i64,
    id: ObjectId,
    commit: &'a Commit,
) -> CommitResponse<'a> {
    let api = &urls.api;
    let web = &urls.web;
    let signature = git.signature(repo_id, id, commit);
    let reason = signature.as_ref().map_or_else(
        || git.verification(commit).as_str(),
        |signature| signature.state.as_str(),
    );
    let account = |author: &hubdouble_git::Signature| {
        author_id(tx, &author.email).and_then(|id| user(tx, urls, id))
    };
    let link = |sha: ObjectId| CommitLink {
        sha,
        url: format!("{api}/repos/{full}/commits/{sha}"),
        html_url: format!("{web}/{full}/commit/{sha}"),
    };
    let node = node_id::encode(
        "C",
        &[
            node_id::Part::Int(0),
            node_id::Part::Int(repo_id),
            node_id::Part::Str(id.to_string()),
        ],
    );
    let (signature, payload) =
        signature.map_or((None, None), |s| (Some(s.signature), Some(s.payload)));
    CommitResponse {
        link: link(id),
        node_id: node,
        comments_url: format!("{api}/repos/{full}/commits/{id}/comments"),
        author: account(&commit.author),
        committer: account(&commit.committer),
        parents: commit.parents.iter().copied().map(link).collect(),
        commit: CommitBody {
            author: (&commit.author).into(),
            committer: (&commit.committer).into(),
            message: &commit.message,
            tree: TreeLink {
                sha: commit.tree,
                url: format!("{api}/repos/{full}/git/trees/{}", commit.tree),
            },
            url: format!("{api}/repos/{full}/git/commits/{id}"),
            comment_count: 0,
            verification: Verification {
                verified: reason == "valid",
                reason,
                signature,
                payload,
                verified_at: (reason == "valid").then_some(commit.committer.time),
            },
        },
    }
}

fn author_id(tx: &Tx, email: &str) -> Option<i64> {
    let accounts = tx.table::<Accounts>();
    let noreply = email
        .strip_suffix("@users.noreply.github.com")
        .and_then(|local| {
            let (id, login) = local
                .split_once('+')
                .map_or((None, local), |(id, login)| (Some(id), login));
            accounts
                .by_login(login)
                .filter(|account| id.is_none_or(|id| id.parse() == Ok(account.id)))
        });
    noreply
        .or_else(|| {
            accounts.iter().find(|a| {
                a.email
                    .as_deref()
                    .is_some_and(|e| e.eq_ignore_ascii_case(email))
            })
        })
        .map(|a| a.id)
}
