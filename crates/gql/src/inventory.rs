use std::path::PathBuf;

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
