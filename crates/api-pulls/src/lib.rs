use hubdouble_admin::Registrar;
use hubdouble_model::{Grant, Requirement, Scope};
use hubdouble_store::StoreBuilder;

mod association;
mod change;
mod events;
mod graphql;
mod merge_state;
mod read;
mod render;
mod reopen;
mod state;
mod timeline;

pub use association::association;
pub use change::kept;
pub use merge_state::mergeable_state;
pub use reopen::{reopen, reopened};
pub use state::sync_refs;
pub use timeline::{StateEvent, StateEvents, StateKind};

const fn repo_requirement(grants: &'static [Grant]) -> Requirement {
    Requirement {
        scope: Scope::Repo,
        any_of: grants,
        scopes: &[],
    }
}

const READ: Requirement = repo_requirement(&[Grant::read("pull_requests")]);
const GET: Requirement = repo_requirement(&[Grant::read("pull_requests"), Grant::read("contents")]);
const WRITE: Requirement = repo_requirement(&[Grant::write("pull_requests")]);
const CONTENTS_WRITE: Requirement = repo_requirement(&[Grant::write("contents")]);

/// Registers the tables that [`association`] and [`mergeable_state`] read, for a server that
/// serves another track's routes without [`register`].
pub fn register_tables(b: &mut StoreBuilder) {
    b.register_table::<merge_state::MergeStates>()
        .register_table::<association::Contributors>();
}

pub fn register(r: &mut Registrar<'_>) {
    r.store.register_table::<state::PullBases>();
    r.store.register_table::<StateEvents>();
    register_tables(r.store);
    r.rest.handle("pulls/list", READ, read::list);
    r.rest.handle("pulls/create", WRITE, change::create);
    r.rest.handle("pulls/get", GET, read::get);
    r.rest.handle("pulls/update", WRITE, change::update);
    r.rest.handle("pulls/list-files", READ, read::files);
    r.rest.handle("pulls/list-commits", READ, read::commits);
    r.rest.handle(
        "repos/list-pull-requests-associated-with-commit",
        READ,
        read::for_commit,
    );
    r.rest.handle("pulls/merge", CONTENTS_WRITE, change::merge);
    graphql::register(r.gql);
    r.git.on_ref_update(state::ref_updated);
    r.admin.kind("pull_request", change::seed);
}
