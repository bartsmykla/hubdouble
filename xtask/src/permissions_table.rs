use std::collections::BTreeSet;
use std::error::Error;
use std::fmt::Write as _;
use std::path::Path;

use serde_json::{Map, Value};

use crate::access_pin::ACCESS;
use crate::{METHODS, Op};

pub(crate) const TABLE: &str = "docs/inventory/rest-permissions.tsv";

const FAMILIES: [&str; 3] = ["installation", "user_to_server", "fine_grained"];

const BARE_SCOPES: &[&str] = &[
    "admin:org",
    "codespace",
    "delete_repo",
    "gist",
    "notifications",
    "project",
    "public_repo",
    "read:org",
    "read:project",
    "read:user",
    "repo",
    "security_events",
    "user",
    "workflow",
    "write:org",
];

const BOTH_KINDS: &[&str] = &[
    "private or public",
    "public or private",
    "private and public",
    "public and private",
    "private or internal",
];

/// The documented access of every operation, keyed by `METHOD /path`.
pub(crate) fn load(root: &Path) -> Result<Map<String, Value>, Box<dyn Error>> {
    let pin: Value = serde_json::from_slice(&std::fs::read(root.join(ACCESS))?)?;
    match pin["operations"].clone() {
        Value::Object(map) => Ok(map),
        _ => Err(format!("{ACCESS} has no operations").into()),
    }
}

pub(crate) fn route(op: &Op) -> String {
    format!("{} {}", METHODS[op.method].0.to_ascii_uppercase(), op.path)
}

/// Whether each token family can call the operation; an operation the docs do not describe
/// keeps every family, since nothing documents a refusal.
pub(crate) fn tokens(access: Option<&Value>) -> [bool; 3] {
    FAMILIES.map(|f| access.is_none_or(|a| a[f].as_bool().unwrap_or(false)))
}

pub(crate) fn write(
    root: &Path,
    ops: &[Op],
    access: &Map<String, Value>,
) -> Result<(), Box<dyn Error>> {
    let mut out = String::from(
        "operation\troute\tinstallation\tuser_to_server\tfine_grained\tpublic_read\tpermissionless\tclassic\n",
    );
    for op in ops {
        let route = route(op);
        let entry = access.get(&route);
        let flag = |field: &str| match entry {
            None => "?",
            Some(a) if a[field].as_bool() == Some(true) => "yes",
            Some(_) => "no",
        };
        let families: Vec<String> = FAMILIES.iter().map(|f| family(entry, f)).collect();
        writeln!(
            out,
            "{}\t{route}\t{}\t{}\t{}\t{}",
            op.id,
            families.join("\t"),
            flag("public_read"),
            flag("permissionless"),
            op.classic
        )?;
    }
    std::fs::write(root.join(TABLE), out)?;
    eprintln!("xtask: wrote {TABLE}");
    Ok(())
}

fn family(entry: Option<&Value>, field: &str) -> String {
    let Some(entry) = entry else {
        return "?".to_owned();
    };
    if entry[field].as_bool() != Some(true) {
        return "x".to_owned();
    }
    let sets: Vec<String> = entry["permission_sets"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_object)
        .map(|set| {
            set.iter()
                .map(|(k, v)| format!("{k}={}", v.as_str().unwrap_or_default()))
                .collect::<Vec<_>>()
                .join("+")
        })
        .collect();
    if sets.is_empty() {
        "-".to_owned()
    } else {
        sets.join("|")
    }
}

/// The classic OAuth scopes that an operation's description names, as space-separated groups.
/// A group is `a|b` when any one scope suffices and `a+b` when all are needed, prefixed with
/// `private:` when the sentence limits it to private repositories and `workflow:` when it
/// applies only to a change of workflow files; `-` when the description names none.
pub(crate) fn classic(description: &str) -> String {
    let text = description.replace('\n', " ");
    let groups: Vec<String> = sentences(&text)
        .into_iter()
        .filter(|s| {
            (s.contains("(classic)") || s.contains("classic PATs") || s.contains("OAuth"))
                && s.contains("scope")
        })
        .filter_map(group)
        .collect();
    if groups.is_empty() {
        "-".to_owned()
    } else {
        groups.join(" ")
    }
}

fn sentences(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let bytes = text.as_bytes();
    for i in 1..bytes.len() {
        if matches!(bytes[i - 1], b'.' | b':') && bytes[i].is_ascii_whitespace() {
            out.push(text[start..i].trim());
            start = i;
        }
    }
    out.push(text[start..].trim());
    out
}

fn is_scope(token: &str) -> bool {
    let mut parts = token.split(':');
    let word = |w: Option<&str>| {
        w.is_some_and(|w| !w.is_empty() && w.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'))
    };
    let first = parts.next();
    match (parts.next(), parts.next()) {
        (None, _) => word(first) && BARE_SCOPES.contains(&token),
        (Some(second), None) => word(first) && word(Some(second)),
        _ => false,
    }
}

fn group(sentence: &str) -> Option<String> {
    let mut scopes = BTreeSet::new();
    let mut outside = String::new();
    for (i, part) in sentence.split('`').enumerate() {
        if i % 2 == 1 {
            if is_scope(part) {
                scopes.insert(part.to_owned());
            }
            outside.push('X');
        } else {
            for word in part.split(|c: char| !(c.is_ascii_alphanumeric() || c == ':' || c == '_')) {
                if BARE_SCOPES.contains(&word) && is_bare_scope_use(part, word) {
                    scopes.insert(word.to_owned());
                }
            }
            outside.push_str(part);
        }
    }
    if scopes.is_empty() {
        return None;
    }
    let words: Vec<&str> = outside.split_whitespace().collect();
    let alternative = words.iter().any(|w| *w == "or" || *w == "either");
    let joint = outside.contains(" both ")
        || outside.contains("X and X")
        || outside.contains("X and the X")
        || outside.contains("scopes and X");
    let condition = if outside.contains("workflow files") {
        "workflow:"
    } else if outside.contains("private") && !BOTH_KINDS.iter().any(|k| outside.contains(k)) {
        "private:"
    } else {
        ""
    };
    let join = if joint && !alternative { "+" } else { "|" };
    Some(format!(
        "{condition}{}",
        scopes.into_iter().collect::<Vec<_>>().join(join)
    ))
}

fn is_bare_scope_use(part: &str, word: &str) -> bool {
    part.contains(&format!("the {word} scope"))
        || part.contains(&format!("the {word} or"))
        || part.contains(&format!("or the {word} scope"))
        || part.contains(&format!("need {word} scope"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_scope_sentence_becomes_a_group() {
        let cases = [
            (
                "Lists things. OAuth app tokens and personal access tokens (classic) need the `repo` scope to use this endpoint.",
                "repo",
            ),
            (
                "OAuth app tokens and personal access tokens (classic) need the `repo` scope to use this endpoint with a private repository.",
                "private:repo",
            ),
            (
                "OAuth app tokens and personal access tokens (classic) need the `read:packages` and `write:packages` scopes to use this endpoint.",
                "read:packages+write:packages",
            ),
            (
                "OAuth app tokens and personal access tokens (classic) need the `repo` scope to use this endpoint with private or public repositories, or the `public_repo` scope to use this endpoint with only public repositories.",
                "public_repo|repo",
            ),
            (
                "OAuth app tokens and personal access tokens (classic) need the `workflow` scope when the resolved target commit modifies workflow files.",
                "workflow:workflow",
            ),
            (
                "OAuth app tokens and personal access tokens (classic) need the repo scope for private or internal repositories, or the public_repo scope for public repositories.",
                "public_repo|repo",
            ),
            ("Gets the authenticated user. No scope is named here.", "-"),
        ];
        for (description, want) in cases {
            assert_eq!(classic(description), want, "{description}");
        }
    }

    #[test]
    fn an_undocumented_operation_keeps_every_family_and_an_unavailable_one_is_x() {
        let documented = serde_json::json!({
            "installation": false, "user_to_server": true, "fine_grained": true,
            "permission_sets": [{ "contents": "read" }, { "administration": "write", "pages": "write" }]
        });
        assert_eq!(tokens(None), [true, true, true]);
        assert_eq!(tokens(Some(&documented)), [false, true, true]);
        assert_eq!(family(Some(&documented), "installation"), "x");
        assert_eq!(
            family(Some(&documented), "user_to_server"),
            "contents=read|administration=write+pages=write"
        );
        assert_eq!(family(None, "fine_grained"), "?");
    }
}
