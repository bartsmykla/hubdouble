use std::collections::{BTreeMap, BTreeSet};

use hubdouble_model::Requirement;
use hubdouble_rest::Entry;
use hubdouble_server::Registries;

const TABLE: &str = include_str!("../../../docs/inventory/rest-permissions.tsv");
const EXTRAS: &str = include_str!("../../../docs/inventory/rest-extra-permissions.tsv");

type Sets = BTreeSet<BTreeSet<String>>;

struct Row<'t> {
    families: [&'t str; 3],
    public_read: bool,
    classic: &'t str,
}

struct Extra<'t> {
    grants: &'t str,
    public_read: bool,
}

fn table() -> BTreeMap<&'static str, Row<'static>> {
    let mut lines = TABLE.lines();
    assert_eq!(
        lines.next(),
        Some(
            "operation\troute\tinstallation\tuser_to_server\tfine_grained\tpublic_read\tpermissionless\tclassic"
        )
    );
    lines
        .map(|line| {
            let cells: Vec<&str> = line.split('\t').collect();
            assert_eq!(cells.len(), 8, "{line}");
            (
                cells[0],
                Row {
                    families: [cells[2], cells[3], cells[4]],
                    public_read: cells[5] == "yes",
                    classic: cells[7],
                },
            )
        })
        .collect()
}

/// The routes outside the bundle: each row names the documented operation or the GitHub
/// behaviour that its grants come from.
fn extras() -> BTreeMap<&'static str, Extra<'static>> {
    let mut lines = EXTRAS.lines();
    assert_eq!(lines.next(), Some("operation\tgrants\tpublic_read\tbasis"));
    lines
        .map(|line| {
            let cells: Vec<&str> = line.split('\t').collect();
            assert_eq!(cells.len(), 4, "{line}");
            assert!(!cells[3].is_empty(), "{line} gives no basis");
            (
                cells[0],
                Extra {
                    grants: cells[1],
                    public_read: cells[2] == "yes",
                },
            )
        })
        .collect()
}

/// GitHub's sets must match, except that an operation GitHub serves to anyone for public
/// resources (`allowsPublicRead`) may also be registered as public: the permission then guards
/// only what is not public, and the handler answers the rest.
fn grants_match(github: &Sets, public_read: bool, ours: &Sets) -> bool {
    github == ours || public_read && ours.is_empty()
}

/// The permission sets of the first token family that GitHub accepts, read as the choke point
/// reads them: `metadata=read` inside a larger set is implied, since every installation holds it,
/// and a set that contains another one adds nothing, as `contents=write+workflows=write` beside
/// `contents=write`, the workflow file case that the branch rules check on their own.
fn documented(cell: &str) -> Sets {
    let mut sets: Sets = cell
        .split('|')
        .filter(|s| *s != "-")
        .map(|set| {
            let grants: BTreeSet<String> = set.split('+').map(str::to_owned).collect();
            if grants.len() > 1 {
                grants
                    .into_iter()
                    .filter(|g| g != "metadata=read")
                    .collect()
            } else {
                grants
            }
        })
        .collect();
    let all = sets.clone();
    sets.retain(|set| !all.iter().any(|other| other != set && other.is_subset(set)));
    sets
}

fn registered(req: &Requirement) -> Sets {
    let grant = |g: &hubdouble_model::Grant| format!("{}={}", g.permission, g.access.as_str());
    let joint: BTreeSet<String> = req.all_of.iter().map(grant).collect();
    if req.any_of.is_empty() {
        return if joint.is_empty() {
            Sets::new()
        } else {
            [joint].into()
        };
    }
    req.any_of
        .iter()
        .map(|g| {
            let mut set = joint.clone();
            set.insert(grant(g));
            set
        })
        .collect()
}

/// The classic scopes that the docs name for every classic token, from the groups of the table
/// that carry no condition: `private:` groups are the visibility rule that `can_see` applies to
/// private repositories, and `workflow:` groups the workflow file check of the branch rules.
/// A registration must accept each of them; it may accept more, because live
/// `X-Accepted-OAuth-Scopes` answers list scopes that the docs prose leaves out (commit cef2f3e).
fn unconditional(classic: &str) -> Result<BTreeSet<&str>, String> {
    let groups: Vec<&str> = classic
        .split(' ')
        .filter(|g| *g != "-" && !g.starts_with("private:") && !g.starts_with("workflow:"))
        .collect();
    match groups.as_slice() {
        [] => Ok(BTreeSet::new()),
        [group] if !group.contains('+') => Ok(group.split('|').collect()),
        _ => Err(format!(
            "needs {classic}, which one list of scopes cannot hold"
        )),
    }
}

fn first_accepted<'r>(row: &Row<'r>) -> &'r str {
    row.families
        .iter()
        .copied()
        .find(|cell| *cell != "x")
        .unwrap_or("-")
}

fn check(entry: &Entry, row: &Row<'_>) -> Vec<String> {
    let mut wrong = Vec::new();
    let cell = first_accepted(row);
    if cell == "?" {
        wrong.push("the pinned docs do not describe it".to_owned());
        return wrong;
    }
    let github = documented(cell);
    let ours = registered(&entry.requirement);
    if !grants_match(&github, row.public_read, &ours) {
        wrong.push(format!("GitHub grants {github:?}, hubdouble {ours:?}"));
    }
    match unconditional(row.classic) {
        Ok(scopes) => {
            let held: BTreeSet<&str> = entry.requirement.scopes.iter().copied().collect();
            if !scopes.is_empty() && !scopes.is_subset(&held) {
                wrong.push(format!(
                    "GitHub classic scopes {scopes:?} ({}), hubdouble {held:?}",
                    row.classic
                ));
            }
        }
        Err(e) => wrong.push(e),
    }
    let [installation, user_to_server, _] = row.families.map(|c| c != "x");
    if entry.tokens.installation != installation || entry.tokens.user_to_server != user_to_server {
        wrong.push(format!(
            "GitHub accepts installation {installation} and user-to-server {user_to_server}, hubdouble {:?}",
            entry.tokens
        ));
    }
    wrong
}

#[test]
fn every_registered_operation_needs_the_access_github_documents() {
    // Given every registration of the whole server, and the table that `mise run gen` writes from the docs pin
    let registry = Registries::new(hubdouble_api::register_all).rest;
    let table = table();
    let extras = extras();

    // When each registration is compared with GitHub's documented access
    let mut mismatches = Vec::new();
    let mut extras_seen = BTreeSet::new();
    for entry in registry.entries() {
        let id = entry.operation_id;
        if let Some(row) = table.get(id) {
            mismatches.extend(check(entry, row).into_iter().map(|e| format!("{id}: {e}")));
        } else if let Some(extra) = extras.get(id) {
            extras_seen.insert(id);
            let ours = registered(&entry.requirement);
            if !grants_match(&documented(extra.grants), extra.public_read, &ours) {
                mismatches.push(format!(
                    "{id}: the extra table grants {}, hubdouble {ours:?}",
                    extra.grants
                ));
            }
        } else {
            mismatches.push(format!("{id}: in neither table"));
        }
    }

    // Then each one matches, and the extra table names only registered routes
    for id in extras.keys().filter(|id| !extras_seen.contains(*id)) {
        mismatches.push(format!("{id}: an extra row without a registration"));
    }
    assert!(mismatches.is_empty(), "\n{}", mismatches.join("\n"));
}

#[test]
fn the_table_covers_every_operation_of_the_pinned_bundle() {
    // Given the generated table and the server's operation list
    let registry = Registries::new(hubdouble_api::register_all).rest;
    let table = table();

    // When the bundle's operations are listed
    let bundle: BTreeSet<&str> = registry
        .operations()
        .filter(|op| !op.extra)
        .map(|op| op.operation_id)
        .collect();

    // Then the table has exactly one row for each
    assert_eq!(table.keys().copied().collect::<BTreeSet<_>>(), bundle);
}

#[test]
fn a_set_that_contains_another_and_implied_metadata_read_drop_out() {
    // Given cells shaped as GitHub documents git refs, team repositories and Pages
    let refs = documented("contents=write|contents=write+workflows=write");
    let team = documented("members=read+metadata=read");
    let pages = documented("pages=write+administration=write");

    // Then only the sets a token must hold remain
    let sets = |list: &[&[&str]]| -> Sets {
        list.iter()
            .map(|s| s.iter().map(|g| (*g).to_owned()).collect())
            .collect()
    };
    assert_eq!(refs, sets(&[&["contents=write"]]));
    assert_eq!(team, sets(&[&["members=read"]]));
    assert_eq!(pages, sets(&[&["administration=write", "pages=write"]]));
}
