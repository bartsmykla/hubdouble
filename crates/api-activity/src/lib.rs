use hubdouble_admin::Registrar;
use hubdouble_model::{Grant, Requirement, Scope};

mod events;

const REPO_METADATA: Requirement = Requirement {
    scope: Scope::Repo,
    any_of: &[Grant::read("metadata")],
    all_of: &[],
    scopes: &[],
};

const ORG_EVENTS: Requirement = Requirement {
    scope: Scope::Org,
    any_of: &[Grant::read("organization_events")],
    all_of: &[],
    scopes: &[],
};

/// Registers what this crate owns: stars, watching, notification threads and the events API.
/// The events come from the activity rows that `hubdouble_hooks::emit` records.
pub fn register(r: &mut Registrar<'_>) {
    let rest = &mut *r.rest;
    let public = Requirement::public();
    rest.handle("activity/list-public-events", public, events::public);
    rest.handle(
        "activity/list-public-events-for-repo-network",
        public,
        events::network,
    );
    rest.handle("activity/list-public-org-events", public, events::org);
    rest.handle("activity/list-repo-events", REPO_METADATA, events::repo);
    rest.handle(
        "activity/list-events-for-authenticated-user",
        public,
        events::user,
    );
    rest.handle(
        "activity/list-public-events-for-user",
        public,
        events::user_public,
    );
    rest.handle(
        "activity/list-org-events-for-authenticated-user",
        ORG_EVENTS,
        events::user_org,
    );
    rest.handle(
        "activity/list-received-events-for-user",
        public,
        events::received,
    );
    rest.handle(
        "activity/list-received-public-events-for-user",
        public,
        events::received_public,
    );
}
