use std::collections::HashMap;
use std::sync::Arc;

use hubdouble_model::{
    Account, App, CheckRun, CheckSuite, Delivery, Installation, Issue, IssueComment, Label,
    Membership, Pull, Reaction, Release, ReleaseAsset, Repo, Review, ReviewComment, ReviewThread,
    Ruleset, Status, Team, Timestamp, Token, WorkflowJob, WorkflowRun,
};

use crate::blobs::Blobs;
use crate::counters::Counters;
use crate::rows::{Entity, Rows};
use crate::store::{StoreBuilder, Table};

pub type Accounts = Rows<Account>;
pub type Memberships = Rows<Membership>;
pub type Teams = Rows<Team>;
pub type Repos = Rows<Repo>;
pub type Apps = Rows<App>;
pub type Installations = Rows<Installation>;
pub type Tokens = Rows<Token>;
pub type Issues = Rows<Issue>;
pub type IssueComments = Rows<IssueComment>;
pub type Reviews = Rows<Review>;
pub type ReviewComments = Rows<ReviewComment>;
pub type ReviewThreads = Rows<ReviewThread>;
pub type Reactions = Rows<Reaction>;
pub type Labels = Rows<Label>;
pub type CheckSuites = Rows<CheckSuite>;
pub type CheckRuns = Rows<CheckRun>;
pub type Statuses = Rows<Status>;
pub type Releases = Rows<Release>;
pub type ReleaseAssets = Rows<ReleaseAsset>;
pub type WorkflowRuns = Rows<WorkflowRun>;
pub type WorkflowJobs = Rows<WorkflowJob>;
pub type Rulesets = Rows<Ruleset>;
pub type Deliveries = Rows<Delivery>;
pub type ActivityEvents = Rows<ActivityEvent>;

pub(crate) fn register_frozen(b: &mut StoreBuilder) {
    b.register_table::<Counters>()
        .register_table::<Blobs>()
        .register_table::<Accounts>()
        .register_table::<Memberships>()
        .register_table::<Teams>()
        .register_table::<Repos>()
        .register_table::<Apps>()
        .register_table::<Installations>()
        .register_table::<Tokens>()
        .register_table::<Issues>()
        .register_table::<IssueComments>()
        .register_table::<Reviews>()
        .register_table::<ReviewComments>()
        .register_table::<ReviewThreads>()
        .register_table::<Reactions>()
        .register_table::<Labels>()
        .register_table::<CheckSuites>()
        .register_table::<CheckRuns>()
        .register_table::<Statuses>()
        .register_table::<Releases>()
        .register_table::<ReleaseAssets>()
        .register_table::<WorkflowRuns>()
        .register_table::<WorkflowJobs>()
        .register_table::<Rulesets>()
        .register_table::<Deliveries>()
        .register_table::<ActivityEvents>()
        .register_table::<RepoRedirects>();
}

/// The old `owner/name` of each renamed or moved repository, which GitHub keeps to redirect
/// until a repository takes that name again. Matched case-insensitively.
#[derive(Clone, Default)]
pub struct RepoRedirects {
    names: HashMap<String, i64>,
}

impl Table for RepoRedirects {}

impl RepoRedirects {
    #[must_use]
    pub fn get(&self, owner: &str, name: &str) -> Option<i64> {
        self.names.get(&full_name(owner, name)).copied()
    }

    /// Records that `repo_id` was `owner/name`.
    pub fn moved(&mut self, owner: &str, name: &str, repo_id: i64) {
        self.names.insert(full_name(owner, name), repo_id);
    }

    /// Drops the redirect of `owner/name`, as a repository now has that name.
    pub fn claim(&mut self, owner: &str, name: &str) {
        self.names.remove(&full_name(owner, name));
    }

    /// Drops every redirect to a deleted repository.
    pub fn forget(&mut self, repo_id: i64) {
        self.names.retain(|_, id| *id != repo_id);
    }
}

fn full_name(owner: &str, name: &str) -> String {
    format!("{owner}/{name}").to_ascii_lowercase()
}

fn pair(a: i64, b: &str) -> String {
    format!("{a}/{}", b.to_ascii_lowercase())
}

impl Entity for Account {
    fn id(&self) -> i64 {
        self.id
    }
    fn node_id(&self) -> Option<&str> {
        Some(&self.node_id)
    }
    fn keys(&self) -> Vec<String> {
        vec![self.login.to_ascii_lowercase()]
    }
}

impl Rows<Account> {
    /// Case-insensitive, as GitHub matches logins.
    #[must_use]
    pub fn by_login(&self, login: &str) -> Option<&Arc<Account>> {
        self.by_key(&login.to_ascii_lowercase())
    }
}

impl Entity for Membership {
    fn id(&self) -> i64 {
        self.id
    }
    fn parent_id(&self) -> Option<i64> {
        Some(self.org_id)
    }
    fn keys(&self) -> Vec<String> {
        vec![format!("{}/{}", self.org_id, self.user_id)]
    }
}

impl Rows<Membership> {
    #[must_use]
    pub fn of(&self, org_id: i64, user_id: i64) -> Option<&Arc<Membership>> {
        self.by_key(&format!("{org_id}/{user_id}"))
    }
}

impl Entity for Team {
    fn id(&self) -> i64 {
        self.id
    }
    fn node_id(&self) -> Option<&str> {
        Some(&self.node_id)
    }
    fn parent_id(&self) -> Option<i64> {
        Some(self.org_id)
    }
    fn keys(&self) -> Vec<String> {
        vec![pair(self.org_id, &self.slug)]
    }
}

impl Rows<Team> {
    #[must_use]
    pub fn by_slug(&self, org_id: i64, slug: &str) -> Option<&Arc<Team>> {
        self.by_key(&pair(org_id, slug))
    }
}

impl Entity for Repo {
    fn id(&self) -> i64 {
        self.id
    }
    fn node_id(&self) -> Option<&str> {
        Some(&self.node_id)
    }
    fn parent_id(&self) -> Option<i64> {
        Some(self.owner_id)
    }
    fn keys(&self) -> Vec<String> {
        vec![pair(self.owner_id, &self.name)]
    }
}

impl Rows<Repo> {
    /// Case-insensitive, as GitHub matches repository names.
    #[must_use]
    pub fn by_name(&self, owner_id: i64, name: &str) -> Option<&Arc<Repo>> {
        self.by_key(&pair(owner_id, name))
    }
}

impl Entity for App {
    fn id(&self) -> i64 {
        self.id
    }
    fn node_id(&self) -> Option<&str> {
        Some(&self.node_id)
    }
    fn keys(&self) -> Vec<String> {
        vec![
            format!("slug:{}", self.slug.to_ascii_lowercase()),
            format!("client:{}", self.client_id),
        ]
    }
}

impl Rows<App> {
    #[must_use]
    pub fn by_slug(&self, slug: &str) -> Option<&Arc<App>> {
        self.by_key(&format!("slug:{}", slug.to_ascii_lowercase()))
    }

    #[must_use]
    pub fn by_client_id(&self, client_id: &str) -> Option<&Arc<App>> {
        self.by_key(&format!("client:{client_id}"))
    }
}

impl Entity for Installation {
    fn id(&self) -> i64 {
        self.id
    }
    fn node_id(&self) -> Option<&str> {
        Some(&self.node_id)
    }
    fn parent_id(&self) -> Option<i64> {
        Some(self.app_id)
    }
    fn keys(&self) -> Vec<String> {
        vec![format!("{}/{}", self.app_id, self.account_id)]
    }
}

impl Rows<Installation> {
    #[must_use]
    pub fn of(&self, app_id: i64, account_id: i64) -> Option<&Arc<Installation>> {
        self.by_key(&format!("{app_id}/{account_id}"))
    }
}

impl Entity for Token {
    fn id(&self) -> i64 {
        self.id
    }
    fn parent_id(&self) -> Option<i64> {
        self.installation_id.or(self.user_id)
    }
    fn keys(&self) -> Vec<String> {
        vec![self.token.clone()]
    }
    fn keep_on_reset(&self) -> bool {
        self.owner_seeded && !self.revoked
    }
}

impl Rows<Token> {
    #[must_use]
    pub fn by_value(&self, token: &str) -> Option<&Arc<Token>> {
        self.by_key(token)
    }
}

impl Entity for Issue {
    fn id(&self) -> i64 {
        self.id
    }
    fn node_id(&self) -> Option<&str> {
        Some(&self.node_id)
    }
    fn alt_id(&self) -> Option<i64> {
        self.pull.as_ref().map(|p| p.id)
    }
    fn alt_node_id(&self) -> Option<&str> {
        self.pull.as_ref().map(|p| p.node_id.as_str())
    }
    fn repo_id(&self) -> Option<i64> {
        Some(self.repo_id)
    }
    fn number(&self) -> Option<i64> {
        Some(self.number)
    }
    fn parent_id(&self) -> Option<i64> {
        self.pull.as_ref()?.head.repo_id
    }
}

impl Rows<Issue> {
    /// Pull requests whose head is in the repository, including pull requests against a fork.
    pub fn pulls_with_head_repo(&self, repo_id: i64) -> impl Iterator<Item = &Arc<Issue>> {
        self.children(repo_id)
    }

    /// Pull requests with a head or base on the branch. Each pull request occurs once.
    pub fn pulls_on_branch<'a>(
        &'a self,
        repo_id: i64,
        branch: &'a str,
    ) -> impl Iterator<Item = &'a Arc<Issue>> {
        let head_matches =
            move |p: &Pull| p.head.repo_id == Some(repo_id) && p.head.ref_name == branch;
        let heads = self
            .pulls_with_head_repo(repo_id)
            .filter(move |issue| issue.pull.as_ref().is_some_and(head_matches));
        let bases = self.in_repo(repo_id).filter(move |issue| {
            issue
                .pull
                .as_ref()
                .is_some_and(|p| p.base.ref_name == branch && !head_matches(p))
        });
        heads.chain(bases)
    }
}

macro_rules! entity {
    ($t:ty, parent: $parent:ident) => {
        impl Entity for $t {
            fn id(&self) -> i64 {
                self.id
            }
            fn node_id(&self) -> Option<&str> {
                Some(&self.node_id)
            }
            fn repo_id(&self) -> Option<i64> {
                Some(self.repo_id)
            }
            fn parent_id(&self) -> Option<i64> {
                Some(self.$parent)
            }
        }
    };
    ($t:ty) => {
        impl Entity for $t {
            fn id(&self) -> i64 {
                self.id
            }
            fn node_id(&self) -> Option<&str> {
                Some(&self.node_id)
            }
            fn repo_id(&self) -> Option<i64> {
                Some(self.repo_id)
            }
        }
    };
}

entity!(IssueComment, parent: issue_id);
entity!(Review, parent: issue_id);
entity!(ReviewComment, parent: issue_id);
entity!(ReviewThread, parent: issue_id);
entity!(Reaction, parent: subject_id);
entity!(CheckSuite);
entity!(CheckRun, parent: check_suite_id);
entity!(Status);
entity!(ReleaseAsset, parent: release_id);
entity!(WorkflowRun, parent: workflow_id);
entity!(WorkflowJob, parent: run_id);

impl Entity for Label {
    fn id(&self) -> i64 {
        self.id
    }
    fn node_id(&self) -> Option<&str> {
        Some(&self.node_id)
    }
    fn repo_id(&self) -> Option<i64> {
        Some(self.repo_id)
    }
    fn keys(&self) -> Vec<String> {
        vec![pair(self.repo_id, &self.name)]
    }
}

impl Rows<Label> {
    /// Case-insensitive, as GitHub matches label names.
    #[must_use]
    pub fn by_name(&self, repo_id: i64, name: &str) -> Option<&Arc<Label>> {
        self.by_key(&pair(repo_id, name))
    }
}

impl Entity for Release {
    fn id(&self) -> i64 {
        self.id
    }
    fn node_id(&self) -> Option<&str> {
        Some(&self.node_id)
    }
    fn repo_id(&self) -> Option<i64> {
        Some(self.repo_id)
    }
    fn keys(&self) -> Vec<String> {
        vec![format!("{}/{}", self.repo_id, self.tag_name)]
    }
}

impl Rows<Release> {
    #[must_use]
    pub fn by_tag(&self, repo_id: i64, tag: &str) -> Option<&Arc<Release>> {
        self.by_key(&format!("{repo_id}/{tag}"))
    }
}

impl Entity for Ruleset {
    fn id(&self) -> i64 {
        self.id
    }
    fn node_id(&self) -> Option<&str> {
        Some(&self.node_id)
    }
    fn repo_id(&self) -> Option<i64> {
        self.repo_id
    }
    fn parent_id(&self) -> Option<i64> {
        Some(self.source_id)
    }
}

impl Entity for Delivery {
    fn id(&self) -> i64 {
        self.id
    }
    fn repo_id(&self) -> Option<i64> {
        self.repo_id
    }
    fn parent_id(&self) -> Option<i64> {
        Some(self.hook_id)
    }
    fn keep_on_reset(&self) -> bool {
        true
    }
}

/// One entry of GitHub's events API: what an actor did in a repository, as the feeds list it.
#[derive(Debug, Clone, PartialEq)]
pub struct ActivityEvent {
    pub id: i64,
    /// GitHub's event type, such as `PushEvent`.
    pub kind: &'static str,
    pub actor_id: i64,
    pub repo_id: i64,
    /// The repository's `owner/name` when the event happened; a later rename keeps it.
    pub repo_name: String,
    /// The organization that owned the repository, if one did.
    pub org_id: Option<i64>,
    pub payload: serde_json::Value,
    /// Whether the repository was public when the event happened.
    pub public: bool,
    pub created_at: Timestamp,
}

impl Entity for ActivityEvent {
    fn id(&self) -> i64 {
        self.id
    }
    fn repo_id(&self) -> Option<i64> {
        Some(self.repo_id)
    }
    fn parent_id(&self) -> Option<i64> {
        Some(self.actor_id)
    }
}
