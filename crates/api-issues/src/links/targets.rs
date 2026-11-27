use std::collections::{BTreeMap, HashMap, btree_map::IntoValues};
use std::sync::Arc;

use hubdouble_model::{Issue, Repo};
use hubdouble_store::{Issues, Repos, Tx};

use super::{IssueLinks, closing_references, referenced_issue};

const MAX_CLOSING_ISSUES: usize = 10;

pub(crate) struct ClosingIssue<'a> {
    pub(crate) issue: &'a Arc<Issue>,
    keyword: bool,
    manual: bool,
}

impl ClosingIssue<'_> {
    pub(crate) fn matches(&self, user_linked_only: bool, exclude_user_linked: bool) -> bool {
        (!user_linked_only && self.keyword) || (!exclude_user_linked && self.manual)
    }
}

type ClosingIssues<'a> = IntoValues<i64, ClosingIssue<'a>>;

pub(crate) fn pull_closing_issues<'a>(tx: &'a Tx, pull: &Issue, repo: &Repo) -> ClosingIssues<'a> {
    let issues = tx.table::<Issues>();
    let manual = tx
        .table::<IssueLinks>()
        .iter()
        .filter(|link| link.pull_issue_id == pull.id)
        .filter_map(|link| issues.get(link.issue_id));
    select(tx, pull, repo, manual)
}

fn select<'a>(
    tx: &'a Tx,
    pull: &Issue,
    repo: &Repo,
    manual: impl Iterator<Item = &'a Arc<Issue>>,
) -> ClosingIssues<'a> {
    let mut targets = BTreeMap::new();
    if let Some(body) = pull.body.as_deref().filter(|_| {
        pull.pull
            .as_ref()
            .is_some_and(|p| p.base.ref_name == repo.default_branch)
    }) {
        for issue in closing_references(body, &tx.config().web_url)
            .filter_map(|(qualifier, number)| referenced_issue(tx, pull.repo_id, qualifier, number))
        {
            targets.insert(
                issue.id,
                ClosingIssue {
                    issue,
                    keyword: true,
                    manual: false,
                },
            );
        }
    }
    for issue in manual {
        targets
            .entry(issue.id)
            .or_insert(ClosingIssue {
                issue,
                keyword: false,
                manual: false,
            })
            .manual = true;
    }
    if targets.len() > MAX_CLOSING_ISSUES {
        let mut manual_slots = targets
            .values()
            .filter(|target| target.manual)
            .count()
            .min(MAX_CLOSING_ISSUES);
        let mut keyword_slots = MAX_CLOSING_ISSUES - manual_slots;
        targets.retain(|_, target| {
            let remaining = if target.manual {
                &mut manual_slots
            } else {
                &mut keyword_slots
            };
            let keep = *remaining > 0;
            *remaining = remaining.saturating_sub(1);
            keep
        });
    }
    targets.into_values()
}

/// The `(pull issue ID, issue ID)` pairs for the closing links of each pull request.
#[must_use]
pub fn closing_links(tx: &Tx) -> Vec<(i64, i64)> {
    let issues = tx.table::<Issues>();
    let repos = tx.table::<Repos>();
    let mut by_pull: HashMap<i64, Vec<_>> = HashMap::new();
    for link in tx.table::<IssueLinks>().iter() {
        if let Some(issue) = issues.get(link.issue_id) {
            by_pull.entry(link.pull_issue_id).or_default().push(issue);
        }
    }
    let mut links = Vec::new();
    for pull in issues.iter().filter(|issue| issue.pull.is_some()) {
        if let Some(repo) = repos.get(pull.repo_id) {
            let manual = by_pull.remove(&pull.id).into_iter().flatten();
            links.extend(select(tx, pull, repo, manual).map(|link| (pull.id, link.issue.id)));
        }
    }
    links
}
