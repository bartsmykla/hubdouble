use hubdouble_admin::Registrar;
use hubdouble_gql::Conn;

mod query;
mod search;

/// Registers what T11 owns: `Query.search` and `SearchResultItemConnection`.
pub fn register(r: &mut Registrar<'_>) {
    r.gql.object_resolved::<Conn>("SearchResultItemConnection");
    r.gql.field("Query", "search", search::search);
    r.gql
        .field("SearchResultItemConnection", "nodes", search::nodes);
    r.gql.field(
        "SearchResultItemConnection",
        "issueCount",
        search::issue_count,
    );
}
