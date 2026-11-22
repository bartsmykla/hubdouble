use hubdouble_model as m;
use serde::Serialize;

use crate::{SimpleUser, Urls};

/// The URL templates every repository shape carries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RepoUrls {
    pub url: String,
    pub archive_url: String,
    pub assignees_url: String,
    pub blobs_url: String,
    pub branches_url: String,
    pub collaborators_url: String,
    pub comments_url: String,
    pub commits_url: String,
    pub compare_url: String,
    pub contents_url: String,
    pub contributors_url: String,
    pub deployments_url: String,
    pub downloads_url: String,
    pub events_url: String,
    pub forks_url: String,
    pub git_commits_url: String,
    pub git_refs_url: String,
    pub git_tags_url: String,
    pub git_url: String,
    pub issue_comment_url: String,
    pub issue_events_url: String,
    pub issues_url: String,
    pub keys_url: String,
    pub labels_url: String,
    pub languages_url: String,
    pub merges_url: String,
    pub milestones_url: String,
    pub notifications_url: String,
    pub pulls_url: String,
    pub releases_url: String,
    pub ssh_url: String,
    pub stargazers_url: String,
    pub statuses_url: String,
    pub subscribers_url: String,
    pub subscription_url: String,
    pub tags_url: String,
    pub teams_url: String,
    pub trees_url: String,
    pub clone_url: String,
    pub mirror_url: Option<String>,
    pub hooks_url: String,
    pub svn_url: String,
}

impl RepoUrls {
    #[must_use]
    pub fn new(urls: &Urls, full_name: &str) -> RepoUrls {
        let a = format!("{}/repos/{full_name}", urls.api);
        let host = urls.web_host();
        RepoUrls {
            archive_url: format!("{a}/{{archive_format}}{{/ref}}"),
            assignees_url: format!("{a}/assignees{{/user}}"),
            blobs_url: format!("{a}/git/blobs{{/sha}}"),
            branches_url: format!("{a}/branches{{/branch}}"),
            collaborators_url: format!("{a}/collaborators{{/collaborator}}"),
            comments_url: format!("{a}/comments{{/number}}"),
            commits_url: format!("{a}/commits{{/sha}}"),
            compare_url: format!("{a}/compare/{{base}}...{{head}}"),
            contents_url: format!("{a}/contents/{{+path}}"),
            contributors_url: format!("{a}/contributors"),
            deployments_url: format!("{a}/deployments"),
            downloads_url: format!("{a}/downloads"),
            events_url: format!("{a}/events"),
            forks_url: format!("{a}/forks"),
            git_commits_url: format!("{a}/git/commits{{/sha}}"),
            git_refs_url: format!("{a}/git/refs{{/sha}}"),
            git_tags_url: format!("{a}/git/tags{{/sha}}"),
            git_url: format!("git://{host}/{full_name}.git"),
            issue_comment_url: format!("{a}/issues/comments{{/number}}"),
            issue_events_url: format!("{a}/issues/events{{/number}}"),
            issues_url: format!("{a}/issues{{/number}}"),
            keys_url: format!("{a}/keys{{/key_id}}"),
            labels_url: format!("{a}/labels{{/name}}"),
            languages_url: format!("{a}/languages"),
            merges_url: format!("{a}/merges"),
            milestones_url: format!("{a}/milestones{{/number}}"),
            notifications_url: format!("{a}/notifications{{?since,all,participating}}"),
            pulls_url: format!("{a}/pulls{{/number}}"),
            releases_url: format!("{a}/releases{{/id}}"),
            ssh_url: format!("git@{host}:{full_name}.git"),
            stargazers_url: format!("{a}/stargazers"),
            statuses_url: format!("{a}/statuses/{{sha}}"),
            subscribers_url: format!("{a}/subscribers"),
            subscription_url: format!("{a}/subscription"),
            tags_url: format!("{a}/tags"),
            teams_url: format!("{a}/teams"),
            trees_url: format!("{a}/git/trees{{/sha}}"),
            clone_url: format!("{}/{full_name}.git", urls.web),
            mirror_url: None,
            hooks_url: format!("{a}/hooks"),
            svn_url: format!("{}/{full_name}", urls.web),
            url: a,
        }
    }
}

/// The actor's role on a repository, in `permissions`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "the fields mirror GitHub's response schema"
)]
pub struct RepoPermissions {
    pub admin: bool,
    pub maintain: bool,
    pub push: bool,
    pub triage: bool,
    pub pull: bool,
}

impl RepoPermissions {
    #[must_use]
    pub fn of(role: m::RepoRole) -> RepoPermissions {
        RepoPermissions {
            admin: role >= m::RepoRole::Admin,
            maintain: role >= m::RepoRole::Maintain,
            push: role >= m::RepoRole::Write,
            triage: role >= m::RepoRole::Triage,
            pull: true,
        }
    }
}

/// GitHub's `repository`, also used where the schema asks for `minimal-repository` and in
/// webhook payloads.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "the fields mirror GitHub's response schema"
)]
pub struct Repository {
    pub id: i64,
    pub node_id: String,
    pub name: String,
    pub full_name: String,
    pub owner: SimpleUser,
    pub private: bool,
    pub html_url: String,
    pub description: Option<String>,
    pub fork: bool,
    #[serde(flatten)]
    pub urls: RepoUrls,
    pub homepage: Option<String>,
    pub language: Option<String>,
    pub forks_count: i64,
    pub stargazers_count: i64,
    pub watchers_count: i64,
    pub size: i64,
    pub default_branch: String,
    pub open_issues_count: i64,
    pub is_template: bool,
    pub topics: Vec<String>,
    pub has_issues: bool,
    pub has_projects: bool,
    pub has_wiki: bool,
    pub has_pages: bool,
    pub has_downloads: bool,
    pub has_discussions: bool,
    pub archived: bool,
    pub disabled: bool,
    pub visibility: &'static str,
    pub pushed_at: Option<m::Timestamp>,
    pub created_at: Option<m::Timestamp>,
    pub updated_at: Option<m::Timestamp>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permissions: Option<RepoPermissions>,
    pub allow_rebase_merge: bool,
    pub allow_squash_merge: bool,
    pub allow_auto_merge: bool,
    pub delete_branch_on_merge: bool,
    pub allow_merge_commit: bool,
    pub allow_forking: bool,
    pub web_commit_signoff_required: bool,
    pub license: Option<serde_json::Value>,
    pub forks: i64,
    pub open_issues: i64,
    pub watchers: i64,
}

impl Repository {
    #[must_use]
    pub fn new(urls: &Urls, r: &m::Repo, owner: &m::Account) -> Repository {
        let full_name = format!("{}/{}", owner.login, r.name);
        Repository {
            id: r.id,
            node_id: r.node_id.clone(),
            name: r.name.clone(),
            owner: SimpleUser::new(urls, owner),
            private: r.is_private(),
            html_url: format!("{}/{full_name}", urls.web),
            description: r.description.clone(),
            fork: r.fork_of.is_some(),
            urls: RepoUrls::new(urls, &full_name),
            full_name,
            homepage: r.homepage.clone(),
            language: r.language.clone(),
            forks_count: 0,
            stargazers_count: 0,
            watchers_count: 0,
            size: 0,
            default_branch: r.default_branch.clone(),
            open_issues_count: 0,
            is_template: r.is_template,
            topics: r.topics.clone(),
            has_issues: r.has_issues,
            has_projects: r.has_projects,
            has_wiki: r.has_wiki,
            has_pages: false,
            has_downloads: true,
            has_discussions: r.has_discussions,
            archived: r.archived,
            disabled: r.disabled,
            visibility: r.visibility.as_str(),
            pushed_at: r.pushed_at,
            created_at: Some(r.created_at),
            updated_at: Some(r.updated_at),
            permissions: None,
            allow_rebase_merge: r.allow_rebase_merge,
            allow_squash_merge: r.allow_squash_merge,
            allow_auto_merge: r.allow_auto_merge,
            delete_branch_on_merge: r.delete_branch_on_merge,
            allow_merge_commit: r.allow_merge_commit,
            allow_forking: true,
            web_commit_signoff_required: false,
            license: None,
            forks: 0,
            open_issues: 0,
            watchers: 0,
        }
    }
}
