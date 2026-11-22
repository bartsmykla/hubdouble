use std::collections::BTreeSet;
use std::path::PathBuf;

use crate::schema::Schema;

pub(crate) fn root() -> PathBuf {
    PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/inventory"))
}

/// Every inventory document as `(file stem, text)`, sorted by name.
pub(crate) fn documents() -> Vec<(String, String)> {
    let mut docs: Vec<(String, String)> = std::fs::read_dir(root().join("graphql"))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let path = e.path();
            let name = path.file_stem()?.to_str()?.to_owned();
            Some((name, std::fs::read_to_string(&path).ok()?))
        })
        .collect();
    docs.sort();
    docs
}

const OWNERS: [&str; 13] = [
    "P0-B", "T1", "T2", "T3", "T4", "T5", "T6", "T7", "T8", "T9", "T10", "T11", "T12",
];

#[test]
fn graphql_fields_lists_every_selected_field_with_an_owner() {
    // Given the committed field inventory and the documents it is generated from
    let schema = match Schema::load() {
        Ok(s) => s,
        Err(e) => panic!("{e}"),
    };
    let mut selected = BTreeSet::new();
    for (name, text) in documents() {
        match schema.selected_fields(&text) {
            Ok(fields) => selected.extend(fields.into_iter().map(|(t, f)| format!("{t}.{f}"))),
            Err(e) => panic!("{name}: {e:?}"),
        }
    }
    let tsv = std::fs::read_to_string(root().join("graphql-fields.tsv")).unwrap_or_default();
    let mut lines = tsv.lines();
    assert_eq!(lines.next(), Some("field\towner\tdocuments"));
    let rows: Vec<Vec<&str>> = lines.map(|l| l.split('\t').collect()).collect();
    // Then it holds exactly those fields, regenerated with
    // `cargo run -p hubdouble-gql --example graphql_fields`
    let listed: BTreeSet<String> = rows.iter().map(|r| r[0].to_owned()).collect();
    assert_eq!(listed, selected);
    // And every row has one owner from the plan's tracks
    for row in &rows {
        assert_eq!(row.len(), 3, "{row:?}");
        assert!(OWNERS.contains(&row[1]), "{} has no owner", row[0]);
    }
}

#[test]
fn selected_fields_names_the_type_each_field_is_written_in() {
    let schema = match Schema::load() {
        Ok(s) => s,
        Err(e) => panic!("{e}"),
    };
    let fields = schema.selected_fields(
        "query($id: ID!) { node(id: $id) { __typename id ...c ... on Issue { title } } }
         fragment c on Comment { body }",
    );
    let fields: Vec<(&str, &str)> = fields
        .as_deref()
        .unwrap_or_default()
        .iter()
        .map(|(t, f)| (t.as_str(), f.as_str()))
        .collect();
    assert_eq!(
        fields,
        vec![
            ("Comment", "body"),
            ("Issue", "title"),
            ("Node", "id"),
            ("Query", "node")
        ]
    );
}
