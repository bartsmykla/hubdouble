use std::sync::Arc;

use hubdouble_git::{Commit, Git, ObjectId, Signature};
use hubdouble_model::{
    ApiError, Issue, Repo, SquashMergeCommitMessage, SquashMergeCommitTitle, Timestamp,
};
use hubdouble_store::{Accounts, Tx};

use crate::state;

pub(crate) fn cannot_merge() -> ApiError {
    ApiError::new(405, "Pull request cannot be merged")
}

pub(crate) fn squash(
    tx: &Tx,
    git: &Git,
    (repo, issue): (&Repo, &Issue),
    (base, head): (ObjectId, ObjectId),
    (title, message): (Option<String>, Option<String>),
) -> Result<ObjectId, ApiError> {
    let commits = pull_commits(tx, git, repo.id, (base, head))?;
    let merged = git
        .test_merge(tx, repo.id, issue.user_id, base, head)?
        .ok_or_else(cannot_merge)?;
    let now = git.now();
    let author = signature(tx, issue.user_id, now)?;
    let message = squash_message(repo, issue, &commits, &author.email, (title, message));
    let mut commit = Commit {
        tree: state::commit_node(git, tx, repo.id, merged)?.commit.tree,
        parents: vec![base],
        author,
        committer: Signature::web_flow(now),
        message,
        gpg_signature: None,
    };
    git.sign(&mut commit)?;
    Ok(git.write_commit(tx, repo.id, &commit)?)
}

pub(crate) fn rebase(
    tx: &Tx,
    git: &Git,
    (repo, merger): (i64, i64),
    (base, head): (ObjectId, ObjectId),
) -> Result<ObjectId, ApiError> {
    let commits = pull_commits(tx, git, repo, (base, head))?;
    let committer = signature(tx, merger, git.now())?;
    let mut tip = base;
    let mut tree = state::commit_node(git, tx, repo, base)?.commit.tree;
    for (id, c) in commits.iter().filter(|(_, c)| c.parents.len() < 2) {
        let stand_in = git.write_commit(
            tx,
            repo,
            &Commit {
                tree,
                parents: c.parents.clone(),
                author: c.author.clone(),
                committer: c.committer.clone(),
                message: String::new(),
                gpg_signature: None,
            },
        )?;
        let picked = git
            .test_merge(tx, repo, merger, stand_in, *id)?
            .ok_or_else(cannot_merge)?;
        tree = state::commit_node(git, tx, repo, picked)?.commit.tree;
        tip = git.write_commit(
            tx,
            repo,
            &Commit {
                tree,
                parents: vec![tip],
                author: c.author.clone(),
                committer: committer.clone(),
                message: c.message.clone(),
                gpg_signature: None,
            },
        )?;
    }
    Ok(tip)
}

fn pull_commits(
    tx: &Tx,
    git: &Git,
    repo: i64,
    (base, head): (ObjectId, ObjectId),
) -> Result<Vec<(ObjectId, Arc<Commit>)>, ApiError> {
    if git.is_ancestor(tx, repo, head, base)? {
        return Err(cannot_merge());
    }
    Ok(git.compare(tx, repo, base, head)?.commits)
}

fn signature(tx: &Tx, account: i64, time: Timestamp) -> Result<Signature, ApiError> {
    let account = tx
        .table::<Accounts>()
        .get(account)
        .ok_or_else(ApiError::not_found)?;
    Ok(Signature {
        name: account
            .name
            .clone()
            .unwrap_or_else(|| account.login.clone()),
        email: account.email.clone().unwrap_or_else(|| {
            format!("{}+{}@users.noreply.github.com", account.id, account.login)
        }),
        time,
        offset_minutes: 0,
    })
}

fn squash_message(
    repo: &Repo,
    issue: &Issue,
    commits: &[(ObjectId, Arc<Commit>)],
    author_email: &str,
    (title, message): (Option<String>, Option<String>),
) -> String {
    let squashed: Vec<&Commit> = commits
        .iter()
        .map(|(_, c)| c.as_ref())
        .filter(|c| c.parents.len() < 2)
        .collect();
    let single = match squashed.as_slice() {
        [only] => Some(*only),
        _ => None,
    };
    let title = title.unwrap_or_else(|| {
        let text = match (repo.squash_merge_commit_title, single) {
            (SquashMergeCommitTitle::CommitOrPrTitle, Some(c)) => {
                c.message.lines().next().unwrap_or_default()
            }
            _ => &issue.title,
        };
        format!("{text} (#{})", issue.number)
    });
    let body = message.unwrap_or_else(|| {
        let body = match (repo.squash_merge_commit_message, single) {
            (SquashMergeCommitMessage::CommitMessages, Some(c)) => c
                .message
                .split_once('\n')
                .map_or("", |(_, rest)| rest)
                .trim()
                .to_owned(),
            (SquashMergeCommitMessage::CommitMessages, None) => squashed
                .iter()
                .map(|c| format!("* {}", c.message.trim_end()))
                .collect::<Vec<_>>()
                .join("\n\n"),
            (SquashMergeCommitMessage::PrBody, _) => issue.body.clone().unwrap_or_default(),
            (SquashMergeCommitMessage::Blank, _) => String::new(),
        };
        with_co_authors(body, commits, author_email)
    });
    if body.is_empty() {
        title
    } else {
        format!("{title}\n\n{body}")
    }
}

fn with_co_authors(body: String, commits: &[(ObjectId, Arc<Commit>)], author: &str) -> String {
    let mut seen = vec![author.to_ascii_lowercase()];
    let mut trailers = Vec::new();
    for (_, c) in commits {
        let email = c.author.email.to_ascii_lowercase();
        if seen.contains(&email) {
            continue;
        }
        seen.push(email);
        let line = format!("Co-authored-by: {} <{}>", c.author.name, c.author.email);
        if !body.contains(&line) {
            trailers.push(line);
        }
    }
    if trailers.is_empty() {
        return body;
    }
    let trailers = trailers.join("\n");
    if body.is_empty() {
        trailers
    } else if ends_with_trailers(&body) {
        format!("{body}\n{trailers}")
    } else {
        format!("{body}\n\n{trailers}")
    }
}

fn ends_with_trailers(body: &str) -> bool {
    body.rsplit("\n\n").next().is_some_and(|paragraph| {
        paragraph.lines().all(|line| {
            line.split_once(": ").is_some_and(|(key, _)| {
                !key.is_empty() && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
            })
        })
    })
}
