use std::collections::BTreeMap;
use std::error::Error;
use std::path::Path;

use reqwest::Client;
use reqwest::header::{ACCEPT, AUTHORIZATION, USER_AGENT};
use serde_json::{Map, Value, json};

pub(crate) const ACCESS: &str = "spec/github-apps/access.json";
const VERSION: &str = "fpt-2026-03-10";
const PERMISSION_FILES: [&str; 2] = [
    "server-to-server-permissions.json",
    "fine-grained-pat-permissions.json",
];

/// Reads the REST reference data of `github/docs` at `commit` and writes, for every documented
/// operation, which token families GitHub accepts and the permission sets it needs.
pub(crate) fn run(
    root: &Path,
    args: &[String],
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<(), Box<dyn Error>> {
    let [commit] = args else {
        return Err("usage: xtask access-pin <github/docs commit SHA>".into());
    };
    if commit.len() != 40 || !commit.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!("{commit:?} is not a full commit SHA").into());
    }
    let token = env("GITHUB_TOKEN").or_else(|| env("GH_TOKEN"));
    let (categories, permissions) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(fetch(commit, token.as_deref()))?;
    let access = build(commit, &categories, &permissions)?;
    let mut text = serde_json::to_string_pretty(&access)?;
    text.push('\n');
    std::fs::write(root.join(ACCESS), text)?;
    eprintln!(
        "xtask: wrote {ACCESS}: {} operations",
        access["operations"].as_object().map_or(0, Map::len)
    );
    crate::generate(root)
}

async fn fetch(
    commit: &str,
    token: Option<&str>,
) -> Result<(Vec<Value>, Vec<Value>), Box<dyn Error>> {
    let client = Client::new();
    let get = |url: String| {
        let mut request = client
            .get(url)
            .header(ACCEPT, "application/json")
            .header(USER_AGENT, "hubdouble-xtask");
        if let Some(token) = token {
            request = request.header(AUTHORIZATION, format!("Bearer {token}"));
        }
        request
    };
    let listing: Value = get(format!(
        "https://api.github.com/repos/github/docs/contents/src/rest/data/{VERSION}?ref={commit}"
    ))
    .send()
    .await?
    .error_for_status()?
    .json()
    .await?;
    let mut categories = Vec::new();
    for file in listing
        .as_array()
        .ok_or("the docs listing is not an array")?
    {
        let name = file["name"].as_str().ok_or("a docs file without a name")?;
        if std::path::Path::new(name).extension() != Some("json".as_ref()) {
            continue;
        }
        let url = format!(
            "https://raw.githubusercontent.com/github/docs/{commit}/src/rest/data/{VERSION}/{name}"
        );
        categories.push(get(url).send().await?.error_for_status()?.json().await?);
    }
    let mut permissions = Vec::new();
    for name in PERMISSION_FILES {
        let url = format!(
            "https://raw.githubusercontent.com/github/docs/{commit}/src/github-apps/data/{VERSION}/{name}"
        );
        permissions.push(get(url).send().await?.error_for_status()?.json().await?);
    }
    Ok((categories, permissions))
}

fn build(
    commit: &str,
    categories: &[Value],
    permissions: &[Value],
) -> Result<Value, Box<dyn Error>> {
    let names = permission_names(permissions)?;
    let mut operations = BTreeMap::new();
    for category in categories {
        let mut found = Vec::new();
        documented(category, &mut found);
        for op in found {
            let verb = op["verb"].as_str().ok_or("an operation without a verb")?;
            let path = op["requestPath"]
                .as_str()
                .ok_or("an operation without a path")?;
            let route = format!("{} {path}", verb.to_ascii_uppercase());
            let entry = access(&op["progAccess"], &names).map_err(|e| format!("{route}: {e}"))?;
            if let Some(before) = operations.insert(route.clone(), entry.clone())
                && before != entry
            {
                return Err(format!("{route} is documented twice with different access").into());
            }
        }
    }
    Ok(json!({
        "source": { "repository": "github/docs", "commit": commit, "version": VERSION },
        "operations": operations,
    }))
}

fn documented<'v>(value: &'v Value, found: &mut Vec<&'v Value>) {
    match value {
        Value::Object(map) if map.contains_key("progAccess") => found.push(value),
        Value::Object(map) => map.values().for_each(|v| documented(v, found)),
        Value::Array(items) => items.iter().for_each(|v| documented(v, found)),
        _ => {}
    }
}

fn permission_names(files: &[Value]) -> Result<BTreeMap<String, String>, Box<dyn Error>> {
    let mut names = BTreeMap::new();
    for file in files {
        for (key, permission) in file
            .as_object()
            .ok_or("a permissions file is not an object")?
        {
            let display = permission["displayTitle"]
                .as_str()
                .ok_or_else(|| format!("{key} has no displayTitle"))?;
            let (kind, title) = display
                .split_once(" permissions for \"")
                .and_then(|(kind, rest)| Some((kind, rest.strip_suffix('"')?)))
                .ok_or_else(|| format!("unexpected displayTitle {display:?}"))?;
            let name = format!("\"{title}\" {} permissions", kind.to_ascii_lowercase());
            if let Some(before) = names.insert(name.clone(), key.clone())
                && before != *key
            {
                return Err(format!("{name} names both {before} and {key}").into());
            }
        }
    }
    Ok(names)
}

fn access(prog: &Value, names: &BTreeMap<String, String>) -> Result<Value, Box<dyn Error>> {
    let flag = |field: &str| prog[field].as_bool().unwrap_or(false);
    let mut sets = Vec::new();
    for set in prog["permissions"]
        .as_array()
        .ok_or("progAccess has no permissions")?
    {
        let mut grants = BTreeMap::new();
        for (name, level) in set.as_object().ok_or("a permission set is not an object")? {
            let key = names
                .get(name)
                .ok_or_else(|| format!("unknown permission {name:?}"))?;
            let level = level.as_str().ok_or("a permission level is not a string")?;
            grants.insert(key.clone(), level.to_owned());
        }
        sets.push(grants);
    }
    Ok(json!({
        "installation": flag("serverToServer"),
        "user_to_server": flag("userToServerRest"),
        "fine_grained": flag("fineGrainedPat"),
        "public_read": flag("allowsPublicRead"),
        "permissionless": flag("allowPermissionlessAccess"),
        "basic_auth": flag("basicAuth"),
        "permission_sets": sets,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names() -> Vec<Value> {
        vec![json!({
            "contents": { "displayTitle": "Repository permissions for \"Contents\"" },
            "workflows": { "displayTitle": "Repository permissions for \"Workflows\"" },
            "members": { "displayTitle": "Organization permissions for \"Members\"" },
        })]
    }

    #[test]
    fn each_documented_operation_keeps_its_token_families_and_permission_sets() {
        // Given a category file shaped as github/docs writes it, with the operation nested in a subcategory
        let category = json!({ "git": { "refs": [{
            "verb": "post",
            "requestPath": "/repos/{owner}/{repo}/git/refs",
            "progAccess": {
                "userToServerRest": true, "serverToServer": true, "fineGrainedPat": false,
                "permissions": [
                    { "\"Contents\" repository permissions": "write" },
                    { "\"Contents\" repository permissions": "write", "\"Workflows\" repository permissions": "write" }
                ]
            }
        }]}});

        // When the pin is built
        let access = build(&"a".repeat(40), &[category], &names()).unwrap();

        // Then the route keeps both sets under their API names, and absent flags are false
        assert_eq!(
            access["operations"]["POST /repos/{owner}/{repo}/git/refs"],
            json!({
                "installation": true, "user_to_server": true, "fine_grained": false,
                "public_read": false, "permissionless": false, "basic_auth": false,
                "permission_sets": [{ "contents": "write" }, { "contents": "write", "workflows": "write" }]
            })
        );
        assert_eq!(access["source"]["version"], VERSION);
    }

    #[test]
    fn a_permission_that_no_permissions_file_names_fails_the_pin() {
        // Given an operation that names a permission the permission files do not define
        let category = json!([{
            "verb": "get", "requestPath": "/x",
            "progAccess": { "permissions": [{ "\"Unknown\" repository permissions": "read" }] }
        }]);

        // When the pin is built
        let built = build(&"a".repeat(40), &[category], &names());

        // Then it fails instead of guessing a name
        assert!(built.is_err());
    }
}
