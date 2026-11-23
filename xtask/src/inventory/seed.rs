use std::collections::BTreeMap;
use std::path::Path;

use serde_json::Value;
use tree_sitter::Parser;

use super::go::{Source, assignments};
use super::{Result, rename};

const CAPABILITY: &str = ".github/runtime-capabilities.json";
const VERSIONS: &[(&str, &str, &str)] = &[
    ("stage", "reviewadmission/runtime.go", "stageRequestVersion"),
    ("source", "corereview/context.go", "ContextVersion"),
    ("candidate", "corereview/types.go", "CandidateVersion"),
    ("result", "corereview/types.go", "AuditVersion"),
    ("deadline", "reviewpolicy/policy.go", "Version"),
    ("answers", "stagecandidate/candidate.go", "Version"),
];

pub(super) fn check(
    root: &Path,
    source: &Path,
    parser: &mut Parser,
    names: &[(String, String)],
) -> Result<()> {
    let mut expected = BTreeMap::new();
    for &(field, path, name) in VERSIONS {
        let source = Source::read(parser, source.join("internal").join(path))?;
        let value = assignments(&source, source.root(), name, usize::MAX)
            .first()
            .and_then(|node| source.string(*node))
            .ok_or_else(|| format!("{path}: missing version constant {name}"))?;
        expected.insert(field, rename(&value, names));
    }
    let entries = entries(root)?;
    let repo = entries
        .iter()
        .find(|entry| {
            entry["kind"] == "repository"
                && entry["owner"] == "octo-org"
                && entry["name"] == "runner"
        })
        .ok_or("the seed has no octo-org/runner repository")?;
    let tagged = repo["commits"].as_array().into_iter().flatten();
    let files = std::iter::once(&repo["files"]).chain(tagged.map(|commit| &commit["files"]));
    let mut found = false;
    for capability in files.filter_map(|files| files[CAPABILITY].as_str()) {
        check_versions(&serde_json::from_str(capability)?, &expected)?;
        found = true;
    }
    if !found {
        return Err(format!("the seed has no {CAPABILITY} file").into());
    }
    Ok(())
}

pub(super) fn entries(root: &Path) -> Result<Vec<Value>> {
    let seed = std::fs::read_to_string(root.join("examples/client.yaml"))?;
    Ok(serde_saphyr::from_str(&seed)?)
}

fn check_versions(capability: &Value, expected: &BTreeMap<&str, String>) -> Result<()> {
    let core = capability["capabilities"]["core_review"]
        .as_object()
        .ok_or("the seed has no core_review capability")?;
    if capability["version"] != 1
        || capability
            .as_object()
            .is_none_or(|fields| fields.len() != 2)
    {
        return Err(format!("{CAPABILITY}: expected capability format version 1").into());
    }
    for (field, value) in expected {
        if !core.contains_key(*field) && matches!(*field, "deadline" | "answers") {
            continue;
        }
        if core.get(*field).and_then(Value::as_str) != Some(value) {
            return Err(format!("{CAPABILITY}: core_review.{field} must be {value:?}").into());
        }
    }
    if let Some(field) = core.keys().find(|key| !expected.contains_key(key.as_str())) {
        return Err(format!("{CAPABILITY}: unknown core_review field {field}").into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_seed_rejects_an_old_core_review_version() {
        let expected = BTreeMap::from([("stage", "v2".to_owned()), ("deadline", "v1".to_owned())]);
        let seed = |stage| json!({"version": 1, "capabilities": {"core_review": {"stage": stage}}});
        assert!(check_versions(&seed("v2"), &expected).is_ok());
        assert!(check_versions(&seed("v1"), &expected).is_err());
    }
}
