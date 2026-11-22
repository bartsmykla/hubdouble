use hubdouble_model as m;
use serde::Serialize;

use crate::{Integration, Repository, SimpleUser, TeamSimple, Urls, escape};

/// GitHub's `label`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Label {
    pub id: i64,
    pub node_id: String,
    pub url: String,
    pub name: String,
    pub description: Option<String>,
    pub color: String,
    pub default: bool,
    pub archived_at: Option<m::Timestamp>,
    pub archived_by: Option<SimpleUser>,
}

impl Label {
    #[must_use]
    pub fn new(urls: &Urls, l: &m::Label, full_name: &str) -> Label {
        Label {
            id: l.id,
            node_id: l.node_id.clone(),
            url: format!("{}/repos/{full_name}/labels/{}", urls.api, escape(&l.name)),
            name: l.name.clone(),
            description: l.description.clone(),
            color: l.color.clone(),
            default: l.default,
            archived_at: None,
            archived_by: None,
        }
    }
}

/// GitHub's `reaction-rollup`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReactionRollup {
    pub url: String,
    pub total_count: i64,
    #[serde(rename = "+1")]
    pub plus_one: i64,
    #[serde(rename = "-1")]
    pub minus_one: i64,
    pub laugh: i64,
    pub confused: i64,
    pub heart: i64,
    pub hooray: i64,
    pub eyes: i64,
    pub rocket: i64,
}

impl ReactionRollup {
    /// The rollup of `reactions` on the subject whose reactions live at `url`.
    #[must_use]
    pub fn new<'a>(
        url: String,
        reactions: impl IntoIterator<Item = &'a m::Reaction>,
    ) -> ReactionRollup {
        let mut r = ReactionRollup {
            url,
            total_count: 0,
            plus_one: 0,
            minus_one: 0,
            laugh: 0,
            confused: 0,
            heart: 0,
            hooray: 0,
            eyes: 0,
            rocket: 0,
        };
        for reaction in reactions {
            r.total_count += 1;
            *match reaction.content {
                m::ReactionContent::ThumbsUp => &mut r.plus_one,
                m::ReactionContent::ThumbsDown => &mut r.minus_one,
                m::ReactionContent::Laugh => &mut r.laugh,
                m::ReactionContent::Confused => &mut r.confused,
                m::ReactionContent::Heart => &mut r.heart,
                m::ReactionContent::Hooray => &mut r.hooray,
                m::ReactionContent::Eyes => &mut r.eyes,
                m::ReactionContent::Rocket => &mut r.rocket,
            } += 1;
        }
        r
    }
}

/// The `pull_request` member of an issue that is a pull request.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct IssuePullLinks {
    pub url: String,
    pub html_url: String,
    pub diff_url: String,
    pub patch_url: String,
    pub merged_at: Option<m::Timestamp>,
}

/// GitHub's `issue`; for a pull request it carries `pull_request` and `draft`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Issue {
    pub id: i64,
    pub node_id: String,
    pub url: String,
    pub repository_url: String,
    pub labels_url: String,
    pub comments_url: String,
    pub events_url: String,
    pub html_url: String,
    pub number: i64,
    pub state: &'static str,
    pub state_reason: Option<&'static str>,
    pub title: String,
    pub body: Option<String>,
    pub user: Option<SimpleUser>,
    pub labels: Vec<Label>,
    pub assignee: Option<SimpleUser>,
    pub assignees: Vec<SimpleUser>,
    pub milestone: Option<serde_json::Value>,
    pub locked: bool,
    pub active_lock_reason: Option<String>,
    pub comments: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pull_request: Option<IssuePullLinks>,
    pub closed_at: Option<m::Timestamp>,
    pub created_at: m::Timestamp,
    pub updated_at: m::Timestamp,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub draft: Option<bool>,
    pub closed_by: Option<SimpleUser>,
    pub author_association: &'static str,
    pub performed_via_github_app: Option<Integration>,
    pub timeline_url: String,
    pub reactions: ReactionRollup,
}

impl Issue {
    #[must_use]
    pub fn new(
        urls: &Urls,
        i: &m::Issue,
        full_name: &str,
        user: Option<SimpleUser>,
        labels: Vec<Label>,
        assignees: Vec<SimpleUser>,
    ) -> Issue {
        let api = format!("{}/repos/{full_name}/issues/{}", urls.api, i.number);
        let web_kind = if i.pull.is_some() { "pull" } else { "issues" };
        Issue {
            id: i.id,
            node_id: i.node_id.clone(),
            repository_url: format!("{}/repos/{full_name}", urls.api),
            labels_url: format!("{api}/labels{{/name}}"),
            comments_url: format!("{api}/comments"),
            events_url: format!("{api}/events"),
            timeline_url: format!("{api}/timeline"),
            reactions: ReactionRollup::new(format!("{api}/reactions"), []),
            html_url: format!("{}/{full_name}/{web_kind}/{}", urls.web, i.number),
            pull_request: i.pull.as_ref().map(|p| IssuePullLinks {
                url: format!("{}/repos/{full_name}/pulls/{}", urls.api, i.number),
                html_url: format!("{}/{full_name}/pull/{}", urls.web, i.number),
                diff_url: format!("{}/{full_name}/pull/{}.diff", urls.web, i.number),
                patch_url: format!("{}/{full_name}/pull/{}.patch", urls.web, i.number),
                merged_at: p.merged_at,
            }),
            draft: i.pull.as_ref().map(|p| p.draft),
            url: api,
            number: i.number,
            state: i.state.as_str(),
            state_reason: i.state_reason.map(m::StateReason::as_str),
            title: i.title.clone(),
            body: i.body.clone(),
            user,
            labels,
            assignee: assignees.first().cloned(),
            assignees,
            milestone: None,
            locked: i.locked,
            active_lock_reason: i.active_lock_reason.clone(),
            comments: 0,
            closed_at: i.closed_at,
            created_at: i.created_at,
            updated_at: i.updated_at,
            closed_by: None,
            author_association: "NONE",
            performed_via_github_app: None,
        }
    }
}

/// GitHub's `issue-comment`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct IssueComment {
    pub id: i64,
    pub node_id: String,
    pub url: String,
    pub html_url: String,
    pub body: String,
    pub user: Option<SimpleUser>,
    pub created_at: m::Timestamp,
    pub updated_at: m::Timestamp,
    pub issue_url: String,
    pub author_association: &'static str,
    pub performed_via_github_app: Option<Integration>,
    pub reactions: ReactionRollup,
}

impl IssueComment {
    /// `is_pull` picks the `/pull/` or `/issues/` page of `html_url`.
    #[must_use]
    pub fn new(
        urls: &Urls,
        c: &m::IssueComment,
        full_name: &str,
        number: i64,
        is_pull: bool,
        user: Option<SimpleUser>,
    ) -> IssueComment {
        let api = format!("{}/repos/{full_name}/issues/comments/{}", urls.api, c.id);
        let web_kind = if is_pull { "pull" } else { "issues" };
        IssueComment {
            id: c.id,
            node_id: c.node_id.clone(),
            html_url: format!(
                "{}/{full_name}/{web_kind}/{number}#issuecomment-{}",
                urls.web, c.id
            ),
            reactions: ReactionRollup::new(format!("{api}/reactions"), []),
            url: api,
            body: c.body.clone(),
            user,
            created_at: c.created_at,
            updated_at: c.updated_at,
            issue_url: format!("{}/repos/{full_name}/issues/{number}", urls.api),
            author_association: "NONE",
            performed_via_github_app: None,
        }
    }
}

/// GitHub's `reaction`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Reaction {
    pub id: i64,
    pub node_id: String,
    pub user: Option<SimpleUser>,
    pub content: &'static str,
    pub created_at: m::Timestamp,
}

impl Reaction {
    #[must_use]
    pub fn new(r: &m::Reaction, user: Option<SimpleUser>) -> Reaction {
        Reaction {
            id: r.id,
            node_id: r.node_id.clone(),
            user,
            content: r.content.as_str(),
            created_at: r.created_at,
        }
    }
}

/// `{"href": ...}` of a `_links` member.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Href {
    pub href: String,
}

/// The `head` or `base` of a pull request.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PullBranch {
    pub label: String,
    #[serde(rename = "ref")]
    pub ref_name: String,
    pub sha: String,
    pub user: SimpleUser,
    pub repo: Option<Repository>,
}

impl PullBranch {
    /// `repo` is `None` when the head repository was deleted.
    #[must_use]
    pub fn new(
        urls: &Urls,
        r: &m::PullRef,
        owner: &m::Account,
        repo: Option<Repository>,
    ) -> PullBranch {
        PullBranch {
            label: format!("{}:{}", owner.login, r.ref_name),
            ref_name: r.ref_name.clone(),
            sha: r.sha.clone(),
            user: SimpleUser::new(urls, owner),
            repo,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PullLinks {
    #[serde(rename = "self")]
    pub self_link: Href,
    pub html: Href,
    pub issue: Href,
    pub comments: Href,
    pub review_comments: Href,
    pub review_comment: Href,
    pub commits: Href,
    pub statuses: Href,
}

/// GitHub's `pull-request`; also the webhook form.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "the fields mirror GitHub's response schema"
)]
pub struct PullRequest {
    pub url: String,
    pub id: i64,
    pub node_id: String,
    pub html_url: String,
    pub diff_url: String,
    pub patch_url: String,
    pub issue_url: String,
    pub commits_url: String,
    pub review_comments_url: String,
    pub review_comment_url: String,
    pub comments_url: String,
    pub statuses_url: String,
    pub number: i64,
    pub state: &'static str,
    pub locked: bool,
    pub title: String,
    pub user: Option<SimpleUser>,
    pub body: Option<String>,
    pub labels: Vec<Label>,
    pub milestone: Option<serde_json::Value>,
    pub active_lock_reason: Option<String>,
    pub created_at: m::Timestamp,
    pub updated_at: m::Timestamp,
    pub closed_at: Option<m::Timestamp>,
    pub merged_at: Option<m::Timestamp>,
    pub merge_commit_sha: Option<String>,
    pub assignee: Option<SimpleUser>,
    pub assignees: Vec<SimpleUser>,
    pub requested_reviewers: Vec<SimpleUser>,
    pub requested_teams: Vec<TeamSimple>,
    pub head: PullBranch,
    pub base: PullBranch,
    #[serde(rename = "_links")]
    pub links: PullLinks,
    pub author_association: &'static str,
    pub auto_merge: Option<serde_json::Value>,
    pub draft: bool,
    pub merged: bool,
    pub mergeable: Option<bool>,
    pub rebaseable: Option<bool>,
    pub mergeable_state: &'static str,
    pub merged_by: Option<SimpleUser>,
    pub comments: i64,
    pub review_comments: i64,
    pub maintainer_can_modify: bool,
    pub commits: i64,
    pub additions: i64,
    pub deletions: i64,
    pub changed_files: i64,
}

impl PullRequest {
    /// `issue` must have `pull` set; `None` otherwise.
    #[must_use]
    pub fn new(
        urls: &Urls,
        issue: &m::Issue,
        full_name: &str,
        user: Option<SimpleUser>,
        head: PullBranch,
        base: PullBranch,
    ) -> Option<PullRequest> {
        let p = issue.pull.as_ref()?;
        let api = format!("{}/repos/{full_name}/pulls/{}", urls.api, issue.number);
        let issue_api = format!("{}/repos/{full_name}/issues/{}", urls.api, issue.number);
        let html = format!("{}/{full_name}/pull/{}", urls.web, issue.number);
        let statuses = format!("{}/repos/{full_name}/statuses/{}", urls.api, p.head.sha);
        let review_comment = format!("{}/repos/{full_name}/pulls/comments{{/number}}", urls.api);
        let href = |s: &str| Href { href: s.to_owned() };
        let links = PullLinks {
            self_link: href(&api),
            html: href(&html),
            issue: href(&issue_api),
            comments: Href {
                href: format!("{issue_api}/comments"),
            },
            review_comments: Href {
                href: format!("{api}/comments"),
            },
            review_comment: href(&review_comment),
            commits: Href {
                href: format!("{api}/commits"),
            },
            statuses: href(&statuses),
        };
        let merged = p.merged_at.is_some();
        Some(PullRequest {
            id: p.id,
            node_id: p.node_id.clone(),
            diff_url: format!("{html}.diff"),
            patch_url: format!("{html}.patch"),
            html_url: html,
            commits_url: format!("{api}/commits"),
            review_comments_url: format!("{api}/comments"),
            review_comment_url: review_comment,
            comments_url: format!("{issue_api}/comments"),
            issue_url: issue_api,
            statuses_url: statuses,
            url: api,
            number: issue.number,
            state: issue.state.as_str(),
            locked: issue.locked,
            title: issue.title.clone(),
            user,
            body: issue.body.clone(),
            labels: Vec::new(),
            milestone: None,
            active_lock_reason: issue.active_lock_reason.clone(),
            created_at: issue.created_at,
            updated_at: issue.updated_at,
            closed_at: issue.closed_at,
            merged_at: p.merged_at,
            merge_commit_sha: p.merge_commit_sha.clone(),
            assignee: None,
            assignees: Vec::new(),
            requested_reviewers: Vec::new(),
            requested_teams: Vec::new(),
            head,
            base,
            links,
            author_association: "NONE",
            auto_merge: None,
            draft: p.draft,
            merged,
            mergeable: p.mergeable,
            rebaseable: p.mergeable,
            mergeable_state: match p.mergeable {
                _ if merged || issue.state == m::IssueState::Closed => "unknown",
                Some(true) => "clean",
                Some(false) => "dirty",
                None => "unknown",
            },
            merged_by: None,
            comments: 0,
            review_comments: 0,
            maintainer_can_modify: p.maintainer_can_modify,
            commits: 0,
            additions: 0,
            deletions: 0,
            changed_files: 0,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PullRequestMinimalRepo {
    pub id: i64,
    pub url: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PullRequestMinimalRef {
    #[serde(rename = "ref")]
    pub ref_name: String,
    pub sha: String,
    pub repo: PullRequestMinimalRepo,
}

/// GitHub's `pull-request-minimal`, in check suites, check runs and workflow runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PullRequestMinimal {
    pub id: i64,
    pub number: i64,
    pub url: String,
    pub head: PullRequestMinimalRef,
    pub base: PullRequestMinimalRef,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReviewLinks {
    pub html: Href,
    pub pull_request: Href,
}

/// GitHub's `pull-request-review`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Review {
    pub id: i64,
    pub node_id: String,
    pub user: Option<SimpleUser>,
    pub body: String,
    pub state: &'static str,
    pub html_url: String,
    pub pull_request_url: String,
    #[serde(rename = "_links")]
    pub links: ReviewLinks,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub submitted_at: Option<m::Timestamp>,
    pub commit_id: Option<String>,
    pub author_association: &'static str,
}

impl Review {
    #[must_use]
    pub fn new(
        urls: &Urls,
        r: &m::Review,
        full_name: &str,
        number: i64,
        user: Option<SimpleUser>,
    ) -> Review {
        let html = format!(
            "{}/{full_name}/pull/{number}#pullrequestreview-{}",
            urls.web, r.id
        );
        let pull = format!("{}/repos/{full_name}/pulls/{number}", urls.api);
        Review {
            id: r.id,
            node_id: r.node_id.clone(),
            user,
            body: r.body.clone(),
            state: r.state.as_str(),
            links: ReviewLinks {
                html: Href { href: html.clone() },
                pull_request: Href { href: pull.clone() },
            },
            html_url: html,
            pull_request_url: pull,
            submitted_at: r.submitted_at,
            commit_id: r.commit_id.clone(),
            author_association: "NONE",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReviewCommentLinks {
    #[serde(rename = "self")]
    pub self_link: Href,
    pub html: Href,
    pub pull_request: Href,
}

/// GitHub's `pull-request-review-comment`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ReviewComment {
    pub url: String,
    pub pull_request_review_id: Option<i64>,
    pub id: i64,
    pub node_id: String,
    pub diff_hunk: String,
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub position: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub original_position: Option<i64>,
    pub commit_id: String,
    pub original_commit_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub in_reply_to_id: Option<i64>,
    pub user: Option<SimpleUser>,
    pub body: String,
    pub created_at: m::Timestamp,
    pub updated_at: m::Timestamp,
    pub html_url: String,
    pub pull_request_url: String,
    pub author_association: &'static str,
    #[serde(rename = "_links")]
    pub links: ReviewCommentLinks,
    pub start_line: Option<i64>,
    pub original_start_line: Option<i64>,
    pub start_side: Option<&'static str>,
    pub line: Option<i64>,
    pub original_line: Option<i64>,
    pub side: Option<&'static str>,
    pub subject_type: &'static str,
    pub reactions: ReactionRollup,
}

impl ReviewComment {
    #[must_use]
    pub fn new(
        urls: &Urls,
        c: &m::ReviewComment,
        full_name: &str,
        number: i64,
        user: Option<SimpleUser>,
    ) -> ReviewComment {
        let api = format!("{}/repos/{full_name}/pulls/comments/{}", urls.api, c.id);
        let html = format!(
            "{}/{full_name}/pull/{number}#discussion_r{}",
            urls.web, c.id
        );
        let pull = format!("{}/repos/{full_name}/pulls/{number}", urls.api);
        ReviewComment {
            pull_request_review_id: c.review_id,
            id: c.id,
            node_id: c.node_id.clone(),
            diff_hunk: c.diff_hunk.clone(),
            path: c.path.clone(),
            position: c.position,
            original_position: c.original_position,
            commit_id: c.commit_id.clone(),
            original_commit_id: c.original_commit_id.clone(),
            in_reply_to_id: c.in_reply_to_id,
            user,
            body: c.body.clone(),
            created_at: c.created_at,
            updated_at: c.updated_at,
            links: ReviewCommentLinks {
                self_link: Href { href: api.clone() },
                html: Href { href: html.clone() },
                pull_request: Href { href: pull.clone() },
            },
            reactions: ReactionRollup::new(format!("{api}/reactions"), []),
            url: api,
            html_url: html,
            pull_request_url: pull,
            author_association: "NONE",
            start_line: c.start_line,
            original_start_line: c.original_start_line,
            start_side: c.start_side.map(m::Side::as_str),
            line: c.line,
            original_line: c.original_line,
            side: c.side.map(m::Side::as_str),
            subject_type: c.subject_type.as_str(),
        }
    }
}
