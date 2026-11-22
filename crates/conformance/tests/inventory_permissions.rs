use std::collections::{BTreeMap, BTreeSet};

use http::Method;
use hubdouble_server::Registries;

const EXPECTED: &str = include_str!("../../../docs/inventory/rest-installation-permissions.tsv");
const ROUTES: &str = include_str!("../../../docs/inventory/rest-routes.txt");

#[test]
fn installation_grants_match_github_for_inventory_operations() {
    let registry = Registries::new(hubdouble_api::register_all).rest;
    let expected: BTreeMap<_, _> = EXPECTED
        .lines()
        .skip(1)
        .map(|line| line.split_once('\t').unwrap())
        .collect();
    let mut mismatches = Vec::new();
    for route in ROUTES.lines() {
        let (verb, path) = route.split_once(' ').unwrap();
        if path == "/graphql" {
            continue;
        }
        let method = Method::from_bytes(verb.as_bytes()).unwrap();
        let target = sample_path(path);
        let Some(entry) = registry
            .route(&method, &target)
            .and_then(|found| found.entry)
        else {
            mismatches.push(format!("{route}: no registration"));
            continue;
        };
        let installed: BTreeSet<_> = entry
            .requirement
            .any_of
            .iter()
            .map(|grant| format!("{}={}", grant.permission, grant.access.as_str()))
            .collect();
        let Some(grants) = expected.get(route) else {
            mismatches.push(format!("{route}: no GitHub permission record"));
            continue;
        };
        let github: BTreeSet<_> = grants
            .split(';')
            .filter(|grant| *grant != "-")
            .map(str::to_owned)
            .collect();
        if installed != github {
            mismatches.push(format!(
                "{route}: GitHub {github:?}, hubdouble {installed:?}"
            ));
        }
    }
    assert_eq!(expected.len(), ROUTES.lines().count() - 1);
    assert!(mismatches.is_empty(), "{}", mismatches.join("\n"));
}

fn sample_path(path: &str) -> String {
    path.split('/')
        .map(|segment| {
            if segment.starts_with('{') {
                "1"
            } else {
                segment
            }
        })
        .collect::<Vec<_>>()
        .join("/")
}
