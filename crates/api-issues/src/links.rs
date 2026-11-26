use std::sync::Arc;

use hubdouble_admin::SeedCtx;
use hubdouble_auth::repo_role;
use hubdouble_git::{Git, ObjectId, RefChange};
use hubdouble_model::{
    Access, ApiError, Issue, IssueState, Repo, RepoRole, RepositorySelection, Timestamp, Visibility,
};
use hubdouble_store::{Accounts, Entity, Installations, Issues, Repos, Rows, Tx, TxMut};
use serde::Deserialize;
use serde_json::Value;

use crate::events::{self, What};
use crate::issues::apply_state;
use crate::shared::emit_issue;

const CLOSING_KEYWORDS: [&str; 9] = [
    "close", "closes", "closed", "fix", "fixes", "fixed", "resolve", "resolves", "resolved",
];

pub(crate) fn closing_references<'t>(
    text: &'t str,
    web: &'t str,
) -> impl Iterator<Item = (&'t str, i64)> {
    let mut previous = "";
    text.split_whitespace().filter_map(move |word| {
        let last = std::mem::replace(&mut previous, word);
        let keyword = last.trim_start_matches('(').trim_end_matches(':');
        if !CLOSING_KEYWORDS
            .iter()
            .any(|k| k.eq_ignore_ascii_case(keyword))
        {
            return None;
        }
        let (at, qualifier, number) = references_in(word.trim_start_matches('('), web).next()?;
        (at == 0).then_some((qualifier, number))
    })
}

fn references<'t>(text: &'t str, web: &'t str) -> impl Iterator<Item = (&'t str, i64)> {
    text.split_whitespace()
        .flat_map(move |word| references_in(word, web).map(|(_, q, n)| (q, n)))
}

fn references_in<'t>(word: &'t str, web: &'t str) -> impl Iterator<Item = (usize, &'t str, i64)> {
    word.char_indices()
        .filter_map(move |(at, _)| reference_at(word, web, at))
}

fn reference_at<'t>(word: &'t str, web: &str, at: usize) -> Option<(usize, &'t str, i64)> {
    let bytes = word.as_bytes();
    let rest = word.get(at..)?;
    let free = |i: usize| i == 0 || bytes.get(i - 1).is_none_or(|b| !word_byte(*b));
    if bytes.get(at) == Some(&b'#') {
        let start = (0..at)
            .rev()
            .take_while(|&i| bytes.get(i).is_some_and(|b| name_byte(*b) || *b == b'/'))
            .last()
            .unwrap_or(at);
        let qualifier = word.get(start..at)?;
        let named = qualifier
            .split_once('/')
            .is_some_and(|(o, n)| !o.is_empty() && !n.is_empty() && !n.contains('/'));
        return (qualifier.is_empty() || named).then_some((
            start,
            qualifier,
            number(rest.get(1..)?)?,
        ));
    }
    if !free(at) {
        return None;
    }
    if rest
        .get(..3)
        .is_some_and(|gh| gh.eq_ignore_ascii_case("gh-"))
    {
        return Some((at, "", number(rest.get(3..)?)?));
    }
    let path = rest.strip_prefix(web)?.strip_prefix('/')?;
    let mut parts = path.splitn(4, '/');
    let (owner, name) = (parts.next()?, parts.next()?);
    let valid = |s: &str| !s.is_empty() && s.bytes().all(name_byte);
    if !valid(owner) || !valid(name) || parts.next()? != "issues" {
        return None;
    }
    let from = at + web.len() + 1;
    let qualifier = word.get(from..from + owner.len() + 1 + name.len())?;
    Some((at, qualifier, number(parts.next()?)?))
}

fn number(digits: &str) -> Option<i64> {
    let len = digits.bytes().take_while(u8::is_ascii_digit).count();
    if digits.as_bytes().get(len).is_some_and(|b| word_byte(*b)) {
        return None;
    }
    digits.get(..len)?.parse().ok()
}

fn word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn name_byte(b: u8) -> bool {
    word_byte(b) || matches!(b, b'-' | b'.')
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IssueLink {
    pub(crate) id: i64,
    pub(crate) repo_id: i64,
    pub(crate) issue_id: i64,
    pub(crate) pull_issue_id: i64,
}

impl Entity for IssueLink {
    fn id(&self) -> i64 {
        self.id
    }
    fn repo_id(&self) -> Option<i64> {
        Some(self.repo_id)
    }
    fn parent_id(&self) -> Option<i64> {
        Some(self.issue_id)
    }
    fn keys(&self) -> Vec<String> {
        vec![format!("{}:{}", self.issue_id, self.pull_issue_id)]
    }
}

pub(crate) type IssueLinks = Rows<IssueLink>;

pub(crate) struct MergeClose {
    pull_issue_id: i64,
}

impl Entity for MergeClose {
    fn id(&self) -> i64 {
        self.pull_issue_id
    }
}

pub(crate) type MergeCloses = Rows<MergeClose>;

pub(crate) struct CommitClose {
    id: i64,
    key: String,
}

impl Entity for CommitClose {
    fn id(&self) -> i64 {
        self.id
    }

    fn keys(&self) -> Vec<String> {
        vec![self.key.clone()]
    }
}

pub(crate) type CommitCloses = Rows<CommitClose>;

pub(crate) fn ref_updated(
    tx: &mut TxMut<'_>,
    git: &Git,
    change: &RefChange,
) -> Result<(), ApiError> {
    let Some(name) = change.name.strip_prefix("refs/heads/") else {
        return Ok(());
    };
    let Some(repo) = tx.table::<Repos>().get(change.repo_id) else {
        return Ok(());
    };
    let default = repo.default_branch == name;
    from_commits(tx, git, change, default)?;
    let issues = tx.table::<Issues>();
    let done = tx.table::<MergeCloses>();
    let merged: Vec<Arc<Issue>> = issues
        .pulls_on_branch(change.repo_id, name)
        .filter(|pull| {
            pull.pull.as_ref().is_some_and(|p| p.merged_at.is_some()) && done.get(pull.id).is_none()
        })
        .cloned()
        .collect();
    for pull in merged {
        tx.table_mut::<MergeCloses>().insert(MergeClose {
            pull_issue_id: pull.id,
        })?;
        let sender = pull
            .pull
            .as_ref()
            .and_then(|p| p.merged_by)
            .unwrap_or(change.sender);
        for issue in closed_by_merge(tx, &pull, sender) {
            close(tx, &issue, sender, git.now(), None)?;
        }
    }
    Ok(())
}

fn from_commits(
    tx: &mut TxMut<'_>,
    git: &Git,
    change: &RefChange,
    default: bool,
) -> Result<(), ApiError> {
    let source = change.repo_id;
    let sender = change.sender;
    let via_app = tx.table::<Accounts>().get(sender).and_then(|a| a.app_id);
    let now = git.now();
    let web = tx.config().web_url.clone();
    for (sha, commit) in git.newly_reachable_commits(tx, change)? {
        let message = (commit.message.as_str(), web.as_str());
        let closed = if default {
            let Some(closed) = close_from(tx, (source, sender, now), sha, message)? else {
                continue;
            };
            closed
        } else {
            Vec::new()
        };
        let mut named: Vec<Arc<Issue>> = references(message.0, message.1)
            .filter_map(|(qualifier, number)| named_issue(tx, source, qualifier, number))
            .filter(|i| !closed.contains(&i.id))
            .filter(|i| i.repo_id == source || can_read(tx, sender, i.repo_id))
            .cloned()
            .collect();
        named.sort_unstable_by_key(|i| i.id);
        named.dedup_by_key(|i| i.id);
        for issue in named {
            hubdouble_api_pulls::referenced(
                tx,
                &issue,
                (sender, via_app),
                (source, sha.to_string()),
                now,
            )?;
        }
    }
    Ok(())
}

fn close_from(
    tx: &mut TxMut<'_>,
    (source, sender, now): (i64, i64, Timestamp),
    sha: ObjectId,
    (message, web): (&str, &str),
) -> Result<Option<Vec<i64>>, ApiError> {
    let mut refs = closing_references(message, web);
    let Some(first) = refs.next() else {
        return Ok(Some(Vec::new()));
    };
    let key = format!("{source}/{sha}");
    if tx.table::<CommitCloses>().by_key(&key).is_some() {
        return Ok(None);
    }
    let mut issues: Vec<_> = std::iter::once(first)
        .chain(refs)
        .filter_map(|(qualifier, number)| referenced_issue(tx, source, qualifier, number))
        .filter(|i| i.state == IssueState::Open)
        .filter(|i| i.repo_id == source || can_push(tx, sender, i.repo_id))
        .cloned()
        .collect();
    issues.sort_unstable_by_key(|i| i.id);
    issues.dedup_by_key(|i| i.id);
    let id = tx.next_id();
    tx.table_mut::<CommitCloses>()
        .insert(CommitClose { id, key })?;
    for issue in &issues {
        close(tx, issue, sender, now, Some((source, sha)))?;
    }
    Ok(Some(issues.iter().map(|i| i.id).collect()))
}

fn can_push(tx: &Tx, sender: i64, repo_id: i64) -> bool {
    reaches(
        tx,
        sender,
        repo_id,
        ("contents", Access::Write, RepoRole::Write),
    )
}

fn can_read(tx: &Tx, sender: i64, repo_id: i64) -> bool {
    tx.table::<Repos>()
        .get(repo_id)
        .is_some_and(|r| r.visibility == Visibility::Public)
        || reaches(
            tx,
            sender,
            repo_id,
            ("metadata", Access::Read, RepoRole::Read),
        )
}

fn reaches(
    tx: &Tx,
    sender: i64,
    repo_id: i64,
    (permission, access, role): (&str, Access, RepoRole),
) -> bool {
    let Some(repo) = tx.table::<Repos>().get(repo_id) else {
        return false;
    };
    match tx.table::<Accounts>().get(sender).and_then(|a| a.app_id) {
        Some(app) => tx
            .table::<Installations>()
            .of(app, repo.owner_id)
            .is_some_and(|i| {
                i.suspended_at.is_none()
                    && (i.repository_selection == RepositorySelection::All
                        || i.repo_ids.contains(&repo.id))
                    && i.permissions.get(permission) >= Some(&access)
            }),
        None => repo_role(tx, sender, repo) >= Some(role),
    }
}

fn closed_by_merge(tx: &Tx, pull: &Issue, sender: i64) -> Vec<Arc<Issue>> {
    let issues = tx.table::<Issues>();
    let Some(repo) = tx.table::<Repos>().get(pull.repo_id) else {
        return Vec::new();
    };
    if pull
        .pull
        .as_ref()
        .is_none_or(|p| p.base.ref_name != repo.default_branch)
    {
        return Vec::new();
    }
    let body = pull.body.as_deref().unwrap_or_default();
    let mut closed: Vec<Arc<Issue>> = closing_references(body, &tx.config().web_url)
        .filter_map(|(qualifier, number)| referenced_issue(tx, repo.id, qualifier, number))
        .chain(
            tx.table::<IssueLinks>()
                .iter()
                .filter(|l| l.pull_issue_id == pull.id)
                .filter_map(|l| issues.get(l.issue_id)),
        )
        .filter(|i| i.state == IssueState::Open)
        .filter(|i| i.repo_id == repo.id || can_push(tx, sender, i.repo_id))
        .cloned()
        .collect();
    closed.sort_by_key(|i| i.id);
    closed.dedup_by_key(|i| i.id);
    closed
}

fn referenced_issue<'a>(
    tx: &'a Tx,
    source: i64,
    qualifier: &str,
    number: i64,
) -> Option<&'a Arc<Issue>> {
    named_issue(tx, source, qualifier, number).filter(|i| i.pull.is_none())
}

fn named_issue<'a>(
    tx: &'a Tx,
    source: i64,
    qualifier: &str,
    number: i64,
) -> Option<&'a Arc<Issue>> {
    let repo = if qualifier.is_empty() {
        source
    } else {
        lookup_repo(tx, qualifier)?.id
    };
    tx.table::<Issues>().by_number(repo, number)
}

fn close(
    tx: &mut TxMut<'_>,
    issue: &Issue,
    sender: i64,
    now: Timestamp,
    commit: Option<(i64, ObjectId)>,
) -> Result<(), ApiError> {
    let repo: Arc<Repo> = tx
        .table::<Repos>()
        .get(issue.repo_id)
        .cloned()
        .ok_or_else(ApiError::not_found)?;
    let mut closed = issue.clone();
    apply_state(&mut closed, Some(IssueState::Closed), None, sender, now);
    closed.updated_at = now;
    tx.table_mut::<Issues>().replace(closed.clone())?;
    let via_app = tx.table::<Accounts>().get(sender).and_then(|a| a.app_id);
    let what = commit.map_or(What::Closed(closed.state_reason), |(repo_id, sha)| {
        What::CommitClosed { repo_id, sha }
    });
    events::record(tx, &closed, sender, via_app, what, now)?;
    emit_issue(tx, &repo, &closed, "closed", sender, Vec::new())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LinkSeed {
    repository: String,
    issue_number: i64,
    pull_repository: Option<String>,
    pull_number: i64,
}

fn lookup_repo<'a>(tx: &'a Tx, full_name: &str) -> Option<&'a Repo> {
    let (owner, name) = full_name.split_once('/')?;
    let owner = tx.table::<Accounts>().by_login(owner)?;
    tx.table::<Repos>()
        .by_name(owner.id, name)
        .map(AsRef::as_ref)
}

/// Each pull request and an issue that it closes, as `(pull issue ID, issue ID)`: a closing
/// keyword in the body of a pull request on its repository's default branch, or a manual link.
#[must_use]
pub fn closing_links(tx: &Tx) -> Vec<(i64, i64)> {
    let issues = tx.table::<Issues>();
    let repos = tx.table::<Repos>();
    let mut links: Vec<(i64, i64)> = tx
        .table::<IssueLinks>()
        .iter()
        .map(|l| (l.pull_issue_id, l.issue_id))
        .collect();
    for pull in issues.iter() {
        let (Some(p), Some(body), Some(source)) =
            (&pull.pull, pull.body.as_deref(), repos.get(pull.repo_id))
        else {
            continue;
        };
        if p.base.ref_name != source.default_branch {
            continue;
        }
        for (qualifier, number) in closing_references(body, &tx.config().web_url) {
            let target = if qualifier.is_empty() {
                Some(source.id)
            } else {
                lookup_repo(tx, qualifier).map(|r| r.id)
            };
            if let Some(issue) = target
                .and_then(|repo| issues.by_number(repo, number))
                .filter(|i| i.pull.is_none())
            {
                links.push((pull.id, issue.id));
            }
        }
    }
    links
}

fn repo_named(tx: &Tx, full_name: &str) -> Result<i64, ApiError> {
    lookup_repo(tx, full_name)
        .map(|repo| repo.id)
        .ok_or_else(|| ApiError::unprocessable("No such repository"))
}

pub(crate) fn seed(ctx: &mut SeedCtx<'_>, body: Value) -> Result<(), ApiError> {
    let seed: LinkSeed =
        serde_json::from_value(body).map_err(|e| ApiError::unprocessable(e.to_string()))?;
    let tx = &mut ctx.tx;
    let repo = repo_named(tx, &seed.repository)?;
    let pull_repo = match &seed.pull_repository {
        Some(name) => repo_named(tx, name)?,
        None => repo,
    };
    let issues = tx.table::<Issues>();
    let issue = issues
        .by_number(repo, seed.issue_number)
        .filter(|i| i.pull.is_none())
        .ok_or_else(|| ApiError::unprocessable("No such issue"))?
        .id;
    let pull = issues
        .by_number(pull_repo, seed.pull_number)
        .filter(|i| i.pull.is_some())
        .ok_or_else(|| ApiError::unprocessable("No such pull request"))?
        .id;
    let id = tx.next_id();
    tx.table_mut::<IssueLinks>().insert(IssueLink {
        id,
        repo_id: repo,
        issue_id: issue,
        pull_issue_id: pull,
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{closing_references, references};

    const WEB: &str = "https://github.com";

    #[test]
    fn closing_references_return_only_the_named_targets() {
        let text = "See #2. (Fixes #12), resolves: Acme/other#13!\nClosed #bad; close #15.\n\
                    Fixes GH-25284 fixes: https://github.com/H3rmt/hyprshell/issues/533 \
                    closes see#16";
        assert_eq!(
            closing_references(text, WEB).collect::<Vec<_>>(),
            [
                ("", 12),
                ("Acme/other", 13),
                ("", 15),
                ("", 25284),
                ("H3rmt/hyprshell", 533)
            ]
        );
    }

    #[test]
    fn references_find_each_complete_reference_in_a_word() {
        for (text, want) in [
            (
                "WIP: add data migration to change regex\n\nThe plan is to add '\\b' or \
                 similar before and after the regex, in order\nto limit the boundaries of a \
                 word.  So strings like 'i#1', '[i#1]', '\ni#1', etc. match but strings like \
                 'gh#cli/cli#1' don't match with the\nregular expression \"i#(\\d+)\".",
                &[("cli/cli", 1)][..],
            ),
            (
                "gh-159207: Bound Tachyon replay memory on Linux and Windows (#162580)",
                &[("", 159_207), ("", 162_580)],
            ),
            (
                "Ref https://github.com/python/mypy/issues/22063",
                &[("python/mypy", 22063)],
            ),
            (
                "#13abc a#14 x/y/z#15 nogh-16 https://example.com/a/b/issues/17",
                &[],
            ),
            (
                "<https://github.com/a/b/issues/18#issuecomment-1> a/b#19,a/b#20",
                &[("a/b", 18), ("a/b", 19), ("a/b", 20)],
            ),
        ] {
            // Given text that names issues, from live commits where GitHub recorded them
            // When its references are read
            let found: Vec<_> = references(text, WEB).collect();
            // Then each complete reference counts once, and no partial one
            assert_eq!(found, want, "{text}");
        }
    }
}
