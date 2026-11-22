mod comments;
mod events;
mod graphql;
mod issues;
mod labels;
mod reactions;
mod shared;

use hubdouble_admin::Registrar;

pub use shared::{comment_view, issue_view};

use crate::shared::{ISSUES_READ, ISSUES_WRITE, READ, WRITE};

/// Registers what T7 owns: the issue event side table, the issue, label, comment and reaction
/// operations, and the `Issue`, `IssueComment` and `Repository.issue` GraphQL types and fields.
pub fn register(r: &mut Registrar<'_>) {
    r.store.register_table::<events::IssueEvents>();

    let rest = &mut *r.rest;
    rest.handle("issues/list-for-repo", ISSUES_READ, issues::list);
    rest.handle("issues/create", ISSUES_WRITE, issues::create);
    rest.handle("issues/get", ISSUES_READ, issues::get);
    rest.handle("issues/update", WRITE, issues::update);
    rest.handle("issues/list-events", READ, events::list);
    rest.handle("issues/add-labels", WRITE, issues::add_labels);
    rest.handle("issues/list-labels-for-repo", READ, labels::list);
    rest.handle("issues/create-label", WRITE, labels::create);
    rest.handle("issues/update-label", WRITE, labels::update);
    rest.handle("issues/list-comments", READ, comments::list);
    rest.handle(
        "issues/list-comments-for-repo",
        READ,
        comments::list_for_repo,
    );
    rest.handle("issues/create-comment", WRITE, comments::create);
    rest.handle("issues/get-comment", READ, comments::get);
    rest.handle("issues/update-comment", WRITE, comments::update);
    rest.handle("issues/delete-comment", WRITE, comments::delete);
    rest.handle("reactions/list-for-issue-comment", READ, reactions::list);
    rest.handle(
        "reactions/create-for-issue-comment",
        ISSUES_WRITE,
        reactions::create,
    );

    graphql::register(r.gql);
}
