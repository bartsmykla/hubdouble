mod admin;
mod admin_teams;
mod events;
mod graphql;
mod hierarchy;
mod rest;
mod view;

use std::collections::BTreeSet;

pub use hierarchy::TeamViewer;

use hubdouble_admin::Registrar;
use hubdouble_model::{Grant, Requirement, Scope};
use hubdouble_store::Table;

#[derive(Clone, Default)]
struct PublicMemberships(BTreeSet<i64>);

impl Table for PublicMemberships {}

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

const ORG_MEMBER: Requirement = Requirement {
    scopes: &["read:org", "repo", "user"],
    ..MEMBERS
};

const TEAMS: Requirement = Requirement {
    scopes: &["admin:org", "read:org", "repo", "user", "write:org"],
    ..MEMBERS
};

const TEAM_MEMBERSHIP: Requirement = Requirement {
    scopes: &["admin:org", "read:org", "repo", "write:org"],
    ..MEMBERS
};

const COLLABORATOR_PERMISSION: Requirement = Requirement {
    scopes: &[],
    ..METADATA
};

const TEAM_REPOSITORY: Requirement = Requirement {
    any_of: MEMBERS.any_of,
    scopes: &[],
    ..METADATA
};

pub fn register(r: &mut Registrar<'_>) {
    r.store.register_table::<PublicMemberships>();
    rest::register(r.rest);
    graphql::register(r.gql);
    admin::register(r.admin);
}
