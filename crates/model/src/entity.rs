//! The frozen entities. Each serializes with camelCase field names, which is the form the
//! GraphQL executor falls back to for a field without a resolver; enums serialize as GraphQL
//! enum values and render to REST through `as_str`. A track adds fields only through its own
//! side tables keyed by entity ID.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::access::Access;
use crate::time::Timestamp;

macro_rules! rest_enum {
    ($(#[$meta:meta])* $name:ident { $($variant:ident => $rest:literal),+ $(,)? }) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(rename_all(serialize = "SCREAMING_SNAKE_CASE"))]
        pub enum $name {
            $(#[serde(alias = $rest)] $variant),+
        }

        impl $name {
            /// The value as GitHub's REST API and webhooks write it.
            #[must_use]
            pub fn as_str(self) -> &'static str {
                match self {
                    $($name::$variant => $rest),+
                }
            }

            #[must_use]
            pub fn from_rest(text: &str) -> Option<$name> {
                match text {
                    $($rest => Some($name::$variant),)+
                    _ => None,
                }
            }
        }
    };
}

rest_enum!(AccountKind { User => "User", Bot => "Bot", Organization => "Organization" });
rest_enum!(OrgRole { Admin => "admin", Member => "member" });
rest_enum!(MembershipState { Active => "active", Pending => "pending" });
rest_enum!(TeamPrivacy { Closed => "closed", Secret => "secret" });
rest_enum!(TeamRole { Member => "member", Maintainer => "maintainer" });
rest_enum!(
    /// A repository role, ordered from least to most access.
    #[derive(PartialOrd, Ord)]
    RepoRole { Read => "read", Triage => "triage", Write => "write", Maintain => "maintain", Admin => "admin" }
);
rest_enum!(Visibility { Public => "public", Private => "private", Internal => "internal" });
rest_enum!(SquashMergeCommitTitle { PrTitle => "PR_TITLE", CommitOrPrTitle => "COMMIT_OR_PR_TITLE" });
rest_enum!(SquashMergeCommitMessage { PrBody => "PR_BODY", CommitMessages => "COMMIT_MESSAGES", Blank => "BLANK" });
rest_enum!(RepositorySelection { All => "all", Selected => "selected" });
rest_enum!(TokenKind { Installation => "installation", User => "user", Refresh => "refresh", Personal => "personal" });
rest_enum!(IssueState { Open => "open", Closed => "closed" });
rest_enum!(StateReason { Completed => "completed", NotPlanned => "not_planned", Reopened => "reopened", Duplicate => "duplicate" });
rest_enum!(ReviewState {
    Pending => "PENDING",
    Commented => "COMMENTED",
    Approved => "APPROVED",
    ChangesRequested => "CHANGES_REQUESTED",
    Dismissed => "DISMISSED",
});
rest_enum!(Side { Left => "LEFT", Right => "RIGHT" });
rest_enum!(SubjectType { Line => "line", File => "file" });
rest_enum!(ReactionContent {
    ThumbsUp => "+1",
    ThumbsDown => "-1",
    Laugh => "laugh",
    Confused => "confused",
    Heart => "heart",
    Hooray => "hooray",
    Rocket => "rocket",
    Eyes => "eyes",
});
rest_enum!(ReactionSubjectKind {
    Issue => "issue",
    IssueComment => "issue_comment",
    ReviewComment => "pull_request_review_comment",
    Release => "release",
});
rest_enum!(RunStatus {
    Queued => "queued",
    InProgress => "in_progress",
    Completed => "completed",
    Waiting => "waiting",
    Requested => "requested",
    Pending => "pending",
});
rest_enum!(Conclusion {
    ActionRequired => "action_required",
    Cancelled => "cancelled",
    Failure => "failure",
    Neutral => "neutral",
    Success => "success",
    Skipped => "skipped",
    Stale => "stale",
    TimedOut => "timed_out",
    StartupFailure => "startup_failure",
});
rest_enum!(StatusState { Error => "error", Failure => "failure", Pending => "pending", Success => "success" });
rest_enum!(AnnotationLevel { Notice => "notice", Warning => "warning", Failure => "failure" });
rest_enum!(RulesetTarget { Branch => "branch", Tag => "tag", Push => "push", Repository => "repository" });
rest_enum!(RulesetSource { Repository => "Repository", Organization => "Organization" });
rest_enum!(Enforcement { Disabled => "disabled", Active => "active", Evaluate => "evaluate" });
rest_enum!(AssetState { Uploaded => "uploaded", Open => "open" });

/// A user, a bot or an organization; all three share one ID space.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Account {
    pub id: i64,
    pub node_id: String,
    pub login: String,
    pub kind: AccountKind,
    pub name: Option<String>,
    pub email: Option<String>,
    pub company: Option<String>,
    pub blog: Option<String>,
    pub location: Option<String>,
    pub bio: Option<String>,
    pub twitter_username: Option<String>,
    pub description: Option<String>,
    pub site_admin: bool,
    /// `false` makes the OAuth token exchange answer `unverified_user_email`.
    pub verified_email: bool,
    /// Organizations only: a user token needs SSO for this org to reach its resources.
    pub sso_enforced: bool,
    /// Bots only: the App the bot account belongs to.
    pub app_id: Option<i64>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

impl Account {
    /// GitHub's `ghost` user, which stands in for the author of content whose account was
    /// deleted. No row of the `Accounts` table holds it.
    #[must_use]
    pub fn ghost() -> Account {
        Account {
            id: 10137,
            node_id: "MDQ6VXNlcjEwMTM3".into(),
            login: "ghost".into(),
            kind: AccountKind::User,
            name: Some("Deleted user".into()),
            email: None,
            company: None,
            blog: None,
            location: None,
            bio: None,
            twitter_username: None,
            description: None,
            site_admin: false,
            verified_email: false,
            sso_enforced: false,
            app_id: None,
            created_at: Timestamp::SEED,
            updated_at: Timestamp::SEED,
        }
    }
}

/// A user's membership of an organization.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Membership {
    pub id: i64,
    pub org_id: i64,
    pub user_id: i64,
    pub role: OrgRole,
    pub state: MembershipState,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TeamMember {
    pub user_id: i64,
    pub role: TeamRole,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TeamRepo {
    pub repo_id: i64,
    pub role: RepoRole,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Team {
    pub id: i64,
    pub node_id: String,
    pub org_id: i64,
    pub name: String,
    pub slug: String,
    pub description: Option<String>,
    pub privacy: TeamPrivacy,
    pub notifications_enabled: bool,
    pub parent_id: Option<i64>,
    pub members: Vec<TeamMember>,
    pub repos: Vec<TeamRepo>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Collaborator {
    pub user_id: i64,
    pub role: RepoRole,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
#[expect(
    clippy::struct_excessive_bools,
    reason = "each flag is a separate GitHub repository setting"
)]
pub struct Repo {
    pub id: i64,
    pub node_id: String,
    pub owner_id: i64,
    pub name: String,
    pub description: Option<String>,
    pub homepage: Option<String>,
    pub language: Option<String>,
    pub topics: Vec<String>,
    pub visibility: Visibility,
    /// The parent repository of a fork; a fork shares its parent's object store.
    pub fork_of: Option<i64>,
    pub default_branch: String,
    pub archived: bool,
    pub disabled: bool,
    pub is_template: bool,
    pub has_issues: bool,
    pub has_projects: bool,
    pub has_wiki: bool,
    pub has_discussions: bool,
    pub allow_merge_commit: bool,
    pub allow_squash_merge: bool,
    pub allow_rebase_merge: bool,
    pub squash_merge_commit_title: SquashMergeCommitTitle,
    pub squash_merge_commit_message: SquashMergeCommitMessage,
    pub allow_auto_merge: bool,
    pub delete_branch_on_merge: bool,
    pub collaborators: Vec<Collaborator>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    pub pushed_at: Option<Timestamp>,
}

impl Repo {
    #[must_use]
    pub fn is_private(&self) -> bool {
        self.visibility != Visibility::Public
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Webhook {
    pub url: String,
    pub secret: Option<String>,
    pub active: bool,
}

/// The ID of the built-in GitHub Actions App. No row of the `Apps` table holds it.
pub const ACTIONS_APP_ID: i64 = 15368;

/// The slug of the built-in GitHub Actions App. No row of the `Apps` table holds it.
pub const ACTIONS_APP_SLUG: &str = "github-actions";

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct App {
    pub id: i64,
    pub node_id: String,
    pub slug: String,
    pub name: String,
    pub owner_id: i64,
    /// The `<slug>[bot]` account that App-made comments and reactions name.
    pub bot_id: i64,
    pub description: Option<String>,
    pub external_url: String,
    pub client_id: String,
    #[serde(skip)]
    pub client_secret: String,
    /// PEM public key the App JWT is checked against.
    #[serde(skip)]
    pub public_key_pem: String,
    #[serde(skip)]
    pub private_key_pem: Option<String>,
    pub webhook: Option<Webhook>,
    pub permissions: BTreeMap<String, Access>,
    pub events: Vec<String>,
    pub device_flow: bool,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Installation {
    pub id: i64,
    pub node_id: String,
    pub app_id: i64,
    pub account_id: i64,
    pub repository_selection: RepositorySelection,
    /// The repositories of a `selected` installation.
    pub repo_ids: Vec<i64>,
    pub permissions: BTreeMap<String, Access>,
    pub events: Vec<String>,
    pub suspended_at: Option<Timestamp>,
    pub suspended_by: Option<i64>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Token {
    pub id: i64,
    /// The secret itself (`ghs_...`, `ghu_...`, `ghr_...`, `ghp_...`).
    #[serde(skip)]
    pub token: String,
    pub kind: TokenKind,
    pub app_id: Option<i64>,
    pub installation_id: Option<i64>,
    pub user_id: Option<i64>,
    /// An installation token narrowed to these repositories; `None` is the installation's set.
    pub repo_ids: Option<Vec<i64>>,
    pub permissions: BTreeMap<String, Access>,
    /// Classic OAuth scopes of a personal token.
    pub scopes: Vec<String>,
    pub expires_at: Option<Timestamp>,
    pub revoked: bool,
    /// The installation or user existed at the seed load, so reset keeps the token.
    pub owner_seeded: bool,
    pub created_at: Timestamp,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PullRef {
    /// `None` when the head repository was deleted.
    pub repo_id: Option<i64>,
    pub ref_name: String,
    pub sha: String,
}

/// The pull request part of an [`Issue`].
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Pull {
    /// The pull request's own ID, distinct from the issue's.
    pub id: i64,
    pub node_id: String,
    pub head: PullRef,
    pub base: PullRef,
    pub draft: bool,
    pub merged_at: Option<Timestamp>,
    pub merge_commit_sha: Option<String>,
    pub merged_by: Option<i64>,
    pub mergeable: Option<bool>,
    /// Whether a rebase merge of the last test merge's base and head would apply and give the
    /// test merge's tree. It counts only while `mergeable` is `Some(true)`, and `None` there
    /// reads as rebaseable.
    pub rebaseable: Option<bool>,
    pub maintainer_can_modify: bool,
    pub requested_reviewers: Vec<i64>,
    pub requested_teams: Vec<i64>,
}

/// An issue or a pull request; both share the number space and the table.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Issue {
    pub id: i64,
    pub node_id: String,
    pub repo_id: i64,
    pub number: i64,
    pub title: String,
    pub body: Option<String>,
    pub user_id: i64,
    pub state: IssueState,
    pub state_reason: Option<StateReason>,
    pub locked: bool,
    pub active_lock_reason: Option<String>,
    pub label_ids: Vec<i64>,
    pub assignee_ids: Vec<i64>,
    pub closed_by: Option<i64>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    pub closed_at: Option<Timestamp>,
    pub last_edited_at: Option<Timestamp>,
    pub pull: Option<Pull>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IssueComment {
    pub id: i64,
    pub node_id: String,
    pub repo_id: i64,
    pub issue_id: i64,
    pub user_id: i64,
    pub body: String,
    /// The App whose installation token made the comment.
    pub via_app_id: Option<i64>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    pub last_edited_at: Option<Timestamp>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Review {
    pub id: i64,
    pub node_id: String,
    pub repo_id: i64,
    /// The pull request's issue ID.
    pub issue_id: i64,
    pub user_id: i64,
    pub body: String,
    pub state: ReviewState,
    pub commit_id: Option<String>,
    pub submitted_at: Option<Timestamp>,
    pub created_at: Timestamp,
    pub last_edited_at: Option<Timestamp>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewThread {
    pub id: i64,
    pub node_id: String,
    pub repo_id: i64,
    pub issue_id: i64,
    pub path: String,
    pub line: Option<i64>,
    pub original_line: Option<i64>,
    pub start_line: Option<i64>,
    pub original_start_line: Option<i64>,
    pub diff_side: Side,
    pub start_diff_side: Option<Side>,
    pub subject_type: SubjectType,
    pub is_resolved: bool,
    pub resolved_by: Option<i64>,
    pub is_outdated: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewComment {
    pub id: i64,
    pub node_id: String,
    pub repo_id: i64,
    pub issue_id: i64,
    pub review_id: Option<i64>,
    pub thread_id: i64,
    pub in_reply_to_id: Option<i64>,
    pub user_id: i64,
    pub body: String,
    pub path: String,
    pub diff_hunk: String,
    pub commit_id: String,
    pub original_commit_id: String,
    pub line: Option<i64>,
    pub original_line: Option<i64>,
    pub start_line: Option<i64>,
    pub original_start_line: Option<i64>,
    pub side: Option<Side>,
    pub start_side: Option<Side>,
    pub position: Option<i64>,
    pub original_position: Option<i64>,
    pub subject_type: SubjectType,
    pub via_app_id: Option<i64>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    pub last_edited_at: Option<Timestamp>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Reaction {
    pub id: i64,
    pub node_id: String,
    pub repo_id: i64,
    pub subject_kind: ReactionSubjectKind,
    pub subject_id: i64,
    pub user_id: i64,
    pub content: ReactionContent,
    pub created_at: Timestamp,
}

/// A deployment environment of a repository.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Environment {
    pub id: i64,
    pub node_id: String,
    pub repo_id: i64,
    pub name: String,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Label {
    pub id: i64,
    pub node_id: String,
    pub repo_id: i64,
    pub name: String,
    /// Six hex digits without `#`.
    pub color: String,
    pub description: Option<String>,
    pub default: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckSuite {
    pub id: i64,
    pub node_id: String,
    pub repo_id: i64,
    pub app_id: i64,
    pub head_branch: Option<String>,
    pub head_sha: String,
    pub before: Option<String>,
    pub after: Option<String>,
    pub status: RunStatus,
    pub conclusion: Option<Conclusion>,
    pub rerequestable: bool,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Annotation {
    pub path: String,
    pub start_line: i64,
    pub end_line: i64,
    pub start_column: Option<i64>,
    pub end_column: Option<i64>,
    pub annotation_level: AnnotationLevel,
    pub message: String,
    pub title: Option<String>,
    pub raw_details: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckOutput {
    pub title: Option<String>,
    pub summary: Option<String>,
    pub text: Option<String>,
    pub annotations: Vec<Annotation>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckAction {
    pub label: String,
    pub description: String,
    pub identifier: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckRun {
    pub id: i64,
    pub node_id: String,
    pub repo_id: i64,
    pub check_suite_id: i64,
    pub app_id: i64,
    pub head_sha: String,
    pub name: String,
    pub external_id: Option<String>,
    pub details_url: Option<String>,
    pub status: RunStatus,
    pub conclusion: Option<Conclusion>,
    pub started_at: Option<Timestamp>,
    pub completed_at: Option<Timestamp>,
    pub output: CheckOutput,
    pub actions: Vec<CheckAction>,
}

/// A commit status.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    pub id: i64,
    pub node_id: String,
    pub repo_id: i64,
    pub sha: String,
    pub state: StatusState,
    pub context: String,
    pub description: Option<String>,
    pub target_url: Option<String>,
    pub creator_id: i64,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Release {
    pub id: i64,
    pub node_id: String,
    pub repo_id: i64,
    pub tag_name: String,
    pub target_commitish: String,
    pub name: Option<String>,
    pub body: Option<String>,
    pub draft: bool,
    pub prerelease: bool,
    pub author_id: i64,
    pub created_at: Timestamp,
    pub published_at: Option<Timestamp>,
    pub updated_at: Timestamp,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReleaseAsset {
    pub id: i64,
    pub node_id: String,
    pub repo_id: i64,
    pub release_id: i64,
    pub name: String,
    pub label: Option<String>,
    pub content_type: String,
    pub size: i64,
    pub digest: Option<String>,
    pub download_count: i64,
    pub state: AssetState,
    pub uploader_id: i64,
    /// The `/_blobs/{id}` entry that holds the bytes.
    pub blob_id: String,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowRun {
    pub id: i64,
    pub node_id: String,
    pub repo_id: i64,
    pub workflow_id: i64,
    pub name: String,
    pub path: String,
    pub display_title: String,
    pub run_number: i64,
    pub run_attempt: i64,
    pub event: String,
    pub status: RunStatus,
    pub conclusion: Option<Conclusion>,
    pub head_branch: Option<String>,
    pub head_sha: String,
    pub head_repo_id: i64,
    pub actor_id: i64,
    pub triggering_actor_id: i64,
    pub check_suite_id: Option<i64>,
    pub pull_request_numbers: Vec<i64>,
    pub inputs: BTreeMap<String, serde_json::Value>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    pub run_started_at: Option<Timestamp>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JobStep {
    pub number: i64,
    pub name: String,
    pub status: RunStatus,
    pub conclusion: Option<Conclusion>,
    pub started_at: Option<Timestamp>,
    pub completed_at: Option<Timestamp>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowJob {
    pub id: i64,
    pub node_id: String,
    pub repo_id: i64,
    pub run_id: i64,
    pub run_attempt: i64,
    pub name: String,
    pub workflow_name: Option<String>,
    pub head_sha: String,
    pub head_branch: Option<String>,
    pub status: RunStatus,
    pub conclusion: Option<Conclusion>,
    pub steps: Vec<JobStep>,
    pub labels: Vec<String>,
    pub runner_id: Option<i64>,
    pub runner_name: Option<String>,
    pub runner_group_id: Option<i64>,
    pub runner_group_name: Option<String>,
    pub check_run_id: Option<i64>,
    /// The `/_blobs/{id}` entry that holds the job log.
    pub log_blob_id: Option<String>,
    pub created_at: Timestamp,
    pub started_at: Option<Timestamp>,
    pub completed_at: Option<Timestamp>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BypassActor {
    pub actor_id: Option<i64>,
    /// `Integration`, `OrganizationAdmin`, `RepositoryRole`, `Team` or `DeployKey`.
    pub actor_type: String,
    /// `always`, `pull_request` or `exempt`.
    pub bypass_mode: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Rule {
    /// GitHub's rule type, such as `pull_request` or `required_status_checks`.
    pub rule_type: String,
    pub parameters: Option<serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Ruleset {
    pub id: i64,
    pub node_id: String,
    pub source_type: RulesetSource,
    /// The repository or organization ID that holds the ruleset.
    pub source_id: i64,
    /// The repository of a repository ruleset.
    pub repo_id: Option<i64>,
    pub name: String,
    pub target: RulesetTarget,
    pub enforcement: Enforcement,
    pub bypass_actors: Vec<BypassActor>,
    /// GitHub's `conditions` object, kept as given.
    pub conditions: Option<serde_json::Value>,
    pub rules: Vec<Rule>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeliveryRequest {
    pub headers: BTreeMap<String, String>,
    /// The JSON body as it was sent; it serializes as that JSON value.
    #[serde(serialize_with = "json_body")]
    pub payload: bytes::Bytes,
}

fn json_body<S: serde::Serializer>(body: &bytes::Bytes, s: S) -> Result<S::Ok, S::Error> {
    serde_json::from_slice::<serde_json::Value>(body)
        .map_err(serde::ser::Error::custom)?
        .serialize(s)
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeliveryResponse {
    pub headers: BTreeMap<String, String>,
    pub payload: Option<String>,
}

/// One logged webhook delivery attempt.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Delivery {
    /// The `int64` delivery ID, assigned at enqueue.
    pub id: i64,
    /// The `X-GitHub-Delivery` UUID; a redelivery keeps it.
    pub guid: String,
    pub app_id: i64,
    pub hook_id: i64,
    pub installation_id: Option<i64>,
    pub repo_id: Option<i64>,
    pub event: String,
    pub action: Option<String>,
    pub url: String,
    pub delivered_at: Timestamp,
    pub redelivery: bool,
    /// Seconds.
    pub duration: f64,
    pub status: String,
    pub status_code: i64,
    pub request: DeliveryRequest,
    pub response: DeliveryResponse,
}
