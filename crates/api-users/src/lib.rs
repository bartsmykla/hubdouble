mod admin;
mod device;
mod graphql;
mod oauth;
mod params;
mod sso;
mod state;
mod users;

use hubdouble_admin::Registrar;

/// Registers what this crate owns: tables, REST operations, GraphQL fields, ref subscribers, seed kinds
/// and admin routes.
pub fn register(r: &mut Registrar<'_>) {
    state::register_tables(r.store);
    users::register(r.rest);
    oauth::register(r.rest);
    device::register(r.rest);
    sso::register(r.rest);
    graphql::register(r.gql);
    admin::register(r.admin);
}
