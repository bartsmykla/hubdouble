//! GitHub's REST shapes. Each struct is written once and serves REST answers and webhook
//! payloads. `Option` fields render as `null` unless GitHub's schema marks the field optional,
//! and `Vec` fields render as `[]` when empty. Constructors fill what the entity holds; counts,
//! nested objects and `author_association` are public fields the owning track sets.

mod account;
mod actions;
mod app;
mod checks;
mod issue;
mod release;
mod repo;
mod ruleset;

pub use account::{
    OrgMembership, OrganizationSimple, SimpleUser, Team, TeamMembership, TeamSimple,
};
pub use actions::{Job, JobStep, WorkflowRun};
pub use app::{
    HookDelivery, HookDeliveryItem, HookDeliveryRequest, HookDeliveryResponse, Installation,
    InstallationToken, Integration,
};
pub use checks::{
    CheckOutput, CheckRun, CheckSuite, CheckSuiteRef, CommitPerson, SimpleCommit, Status,
};
pub use issue::{
    Href, Issue, IssueComment, IssuePullLinks, Label, PullBranch, PullLinks, PullRequest,
    PullRequestMinimal, PullRequestMinimalRef, PullRequestMinimalRepo, Reaction, ReactionRollup,
    Review, ReviewComment, ReviewCommentLinks, ReviewLinks,
};
pub use release::{Release, ReleaseAsset};
pub use repo::{RepoPermissions, RepoUrls, Repository};
pub use ruleset::{Ruleset, RulesetBypassActor, RulesetLinks, RulesetRule};

use hubdouble_model::Config;
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};

/// The origins a response is written with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Urls {
    /// `HUBDOUBLE_BASE_URL` plus the API prefix the request came in on (empty or `/api/v3`).
    pub api: String,
    /// `HUBDOUBLE_WEB_URL`, for `html_url` and other links to resources.
    pub web: String,
    /// `HUBDOUBLE_BASE_URL`, for pages hubdouble serves itself.
    pub base: String,
}

impl Urls {
    #[must_use]
    pub fn new(config: &Config, api_prefix: &str) -> Urls {
        Urls {
            api: format!("{}{api_prefix}", config.base_url),
            web: config.web_url.clone(),
            base: config.base_url.clone(),
        }
    }

    /// The host of the web origin, for `git://` and `ssh` clone URLs.
    #[must_use]
    pub fn web_host(&self) -> &str {
        let rest = self
            .web
            .split_once("://")
            .map_or(self.web.as_str(), |(_, r)| r);
        rest.split('/').next().unwrap_or(rest)
    }
}

const SEGMENT: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'~');

/// Escapes one path segment, as GitHub writes label names and asset names into URLs.
#[must_use]
pub fn escape(segment: &str) -> String {
    utf8_percent_encode(segment, SEGMENT).to_string()
}

/// Escapes one query name or value, as GitHub writes them into a `Link` header: a space is `+`.
#[must_use]
pub fn query_escape(part: &str) -> String {
    escape(part).replace("%20", "+")
}

fn escaped(segment: &str) -> impl std::fmt::Display {
    utf8_percent_encode(segment, SEGMENT)
}

fn sized(args: std::fmt::Arguments<'_>) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(128);
    let _ = out.write_fmt(args);
    out
}

macro_rules! url {
    ($($arg:tt)*) => {
        $crate::sized(format_args!($($arg)*))
    };
}
use url;

#[cfg(test)]
mod tests;
