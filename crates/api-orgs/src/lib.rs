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
use hubdouble_model::{ApiError, Grant, Requirement, Scope};
use hubdouble_store::{Deleted, Memberships, Table, TxMut};

#[derive(Clone, Default)]
struct PublicMemberships(BTreeSet<i64>);

impl Table for PublicMemberships {}

const MEMBERS: Requirement = Requirement {
    any_of: &[Grant::read("members")],
    all_of: &[],
    scope: Scope::Org,
    scopes: &["read:org"],
};

const METADATA: Requirement = Requirement {
    any_of: &[Grant::read("metadata")],
    all_of: &[],
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
    all_of: &[],
    scopes: &[],
    ..METADATA
};

#[expect(
    clippy::unnecessary_wraps,
    reason = "a deletion subscriber returns a Result"
)]
fn forget(tx: &mut TxMut<'_>, deleted: &Deleted) -> Result<(), ApiError> {
    let Deleted::Account(account) = deleted else {
        return Ok(());
    };
    let gone: Vec<i64> = tx
        .table::<Memberships>()
        .iter()
        .filter(|m| m.org_id == account.id || m.user_id == account.id)
        .map(|m| m.id)
        .filter(|id| tx.table::<PublicMemberships>().0.contains(id))
        .collect();
    if !gone.is_empty() {
        let mut public = tx.table_mut::<PublicMemberships>();
        for id in gone {
            public.get_mut().0.remove(&id);
        }
    }
    Ok(())
}

pub fn register(r: &mut Registrar<'_>) {
    r.store
        .register_table::<PublicMemberships>()
        .on_delete(forget);
    rest::register(r.rest);
    graphql::register(r.gql);
    admin::register(r.admin);
}
