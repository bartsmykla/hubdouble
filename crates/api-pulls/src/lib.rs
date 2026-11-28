use hubdouble_admin::Registrar;
use hubdouble_model::{ApiError, Grant, Requirement, Scope};
use hubdouble_store::{Deleted, Issues, Reviews, StoreBuilder, TxMut};

mod association;
mod change;
mod events;
mod graphql;
mod mentions;
mod merge_state;
mod mergeability;
mod merging;
mod read;
mod render;
mod reopen;
mod review_requests;
mod reviewing;
mod state;
mod timeline;

pub use association::association;
pub use change::kept;
pub use mentions::record_mentions;
pub use merge_state::mergeable_state;
pub use reopen::{reopen, reopened};
pub use review_requests::{Reviewer, change_reviewers};
pub use reviewing::{review_decision, review_submitted};
pub use state::sync_refs;
pub use timeline::{StateEvent, StateEvents, StateKind, referenced, review_dismissed, via_app};

const fn repo_requirement(grants: &'static [Grant]) -> Requirement {
    Requirement {
        scope: Scope::Repo,
        any_of: grants,
        all_of: &[],
        scopes: &[],
    }
}

const READ: Requirement = repo_requirement(&[Grant::read("pull_requests")]);
const GET: Requirement = repo_requirement(&[Grant::read("pull_requests"), Grant::read("contents")]);
const WRITE: Requirement = repo_requirement(&[Grant::write("pull_requests")]);
const CONTENTS_WRITE: Requirement = repo_requirement(&[Grant::write("contents")]);

/// Registers the tables that [`association`], [`mergeable_state`] and [`change_reviewers`]
/// read or write, [`StateEvents`] included, for a server that serves another track's routes
/// without [`register`].
pub fn register_tables(b: &mut StoreBuilder) {
    b.register_table::<StateEvents>()
        .register_table::<state::PullBases>()
        .register_table::<merge_state::MergeStates>()
        .register_table::<reviewing::LastPushes>()
        .register_table::<reviewing::ReviewOrder>()
        .register_table::<association::Contributors>()
        .on_delete(forget);
}

#[expect(
    clippy::unnecessary_wraps,
    reason = "a deletion subscriber returns a Result"
)]
fn forget(tx: &mut TxMut<'_>, deleted: &Deleted) -> Result<(), ApiError> {
    let Deleted::Repo(repo) = deleted else {
        return Ok(());
    };
    let issues: Vec<i64> = tx
        .table::<Issues>()
        .in_repo(repo.id)
        .map(|i| i.id)
        .collect();
    let reviews: Vec<i64> = tx
        .table::<Reviews>()
        .in_repo(repo.id)
        .map(|r| r.id)
        .collect();
    tx.table_mut::<StateEvents>().remove_in_repo(repo.id);
    tx.table_mut::<state::PullBases>()
        .remove_all(issues.iter().copied());
    tx.table_mut::<merge_state::MergeStates>()
        .remove_all(issues.iter().copied());
    tx.table_mut::<reviewing::LastPushes>()
        .remove_all(issues.iter().copied());
    if tx.try_table::<mergeability::MergeJobs>().is_some() {
        tx.table_mut::<mergeability::MergeJobs>().remove_all(issues);
        tx.table_mut::<mergeability::MergeReads>()
            .remove_all([repo.id]);
    }
    tx.table_mut::<association::Contributors>()
        .remove_all([repo.id]);
    reviewing::forget_reviews(tx, &reviews);
    Ok(())
}

pub fn register(r: &mut Registrar<'_>) {
    register_tables(r.store);
    mergeability::register(r);
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
