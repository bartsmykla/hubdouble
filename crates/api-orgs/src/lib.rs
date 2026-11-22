mod admin;
mod admin_teams;
mod events;
mod graphql;
mod hierarchy;
mod rest;
mod view;

use hubdouble_admin::Registrar;
use hubdouble_model::{Grant, Requirement, Scope};

const MEMBERS: Requirement = Requirement {
    any_of: &[Grant::read("members")],
    scope: Scope::Org,
    scopes: &["read:org"],
};

const METADATA: Requirement = Requirement {
    any_of: &[Grant::read("metadata")],
    scope: Scope::Repo,
    scopes: &["repo", "public_repo"],
};

const TEAM_REPOSITORY: Requirement = Requirement {
    any_of: MEMBERS.any_of,
    scopes: &[],
    ..METADATA
};

pub fn register(r: &mut Registrar<'_>) {
    rest::register(r.rest);
    graphql::register(r.gql);
    admin::register(r.admin);
}
