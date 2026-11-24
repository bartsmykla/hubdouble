use std::error::Error;
use std::path::Path;

use reqwest::Client;
use reqwest::header::{ACCEPT, AUTHORIZATION, USER_AGENT};
use serde_json::{Map, Value, json};

pub(crate) const INTROSPECTION: &str = "spec/graphql/introspection.json";
pub(crate) const EXTRA: &str = "spec/graphql/introspection-extra.json";
const ENDPOINT: &str = "https://api.github.com/graphql";

const TYPE_REF: &str = "type { kind name ofType { kind name ofType { kind name ofType { kind name \
                        ofType { kind name ofType { kind name ofType { kind name } } } } } } }";

fn extra_query() -> String {
    let value =
        format!("{{ name description {TYPE_REF} defaultValue isDeprecated deprecationReason }}");
    format!(
        "{{ __schema {{ description directives {{ name isRepeatable args(includeDeprecated: true) \
         {value} }} types {{ name kind specifiedByURL isOneOf inputFields(includeDeprecated: true) \
         {value} fields(includeDeprecated: true) {{ name args(includeDeprecated: true) {value} }} }} }} }}"
    )
}

/// Reads live `GET /graphql` and one introspection read with `POST /graphql`, and writes the two
/// pins; `env` gives the token as `GITHUB_TOKEN` or `GH_TOKEN`.
pub(crate) fn run(root: &Path, env: &dyn Fn(&str) -> Option<String>) -> Result<(), Box<dyn Error>> {
    let token = env("GITHUB_TOKEN")
        .or_else(|| env("GH_TOKEN"))
        .ok_or("set GITHUB_TOKEN or GH_TOKEN to a token that can read the GraphQL API")?;
    let (introspection, answer) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(fetch(&token))?;
    let extra = extra(&serde_json::from_str(&answer)?)?;
    std::fs::write(
        root.join(INTROSPECTION),
        format!("{}\n", introspection.trim_end()),
    )?;
    let mut extra = serde_json::to_string_pretty(&extra)?;
    extra.push('\n');
    std::fs::write(root.join(EXTRA), extra)?;
    eprintln!("xtask: wrote {INTROSPECTION} and {EXTRA}");
    crate::write_graphql_schema(root)
}

async fn fetch(token: &str) -> Result<(String, String), Box<dyn Error>> {
    let client = Client::new();
    let auth = format!("Bearer {token}");
    let get = client
        .get(ENDPOINT)
        .header(AUTHORIZATION, &auth)
        .header(ACCEPT, "application/json")
        .header(USER_AGENT, "hubdouble-xtask")
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    let post = client
        .post(ENDPOINT)
        .header(AUTHORIZATION, &auth)
        .header(USER_AGENT, "hubdouble-xtask")
        .json(&json!({ "query": extra_query() }))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    Ok((get, post))
}

fn deprecated(values: &Value) -> Vec<Value> {
    values
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
        .filter(|(_, v)| v["isDeprecated"] == true)
        .map(|(index, v)| json!({ "index": index, "value": v }))
        .collect()
}

fn extra(answer: &Value) -> Result<Value, Box<dyn Error>> {
    let schema = &answer["data"]["__schema"];
    let types = schema["types"]
        .as_array()
        .ok_or("the introspection read answered no types")?;
    let mut type_facts = Map::new();
    let mut inputs = Map::new();
    let mut args = Map::new();
    for t in types {
        let name = t["name"].as_str().ok_or("a type has no name")?;
        let mut facts = Map::new();
        if !t["specifiedByURL"].is_null() {
            facts.insert("specifiedByURL".into(), t["specifiedByURL"].clone());
        }
        if t["isOneOf"] != false {
            facts.insert("isOneOf".into(), t["isOneOf"].clone());
        }
        if !facts.is_empty() {
            type_facts.insert(name.into(), Value::Object(facts));
        }
        let fields = deprecated(&t["inputFields"]);
        if !fields.is_empty() {
            inputs.insert(name.into(), Value::Array(fields));
        }
        for f in t["fields"].as_array().into_iter().flatten() {
            let found = deprecated(&f["args"]);
            if !found.is_empty() {
                let field = f["name"].as_str().ok_or("a field has no name")?;
                args.insert(format!("{name}.{field}"), Value::Array(found));
            }
        }
    }
    let mut directives = Map::new();
    for d in schema["directives"].as_array().into_iter().flatten() {
        let name = d["name"].as_str().ok_or("a directive has no name")?;
        directives.insert(name.into(), json!({ "isRepeatable": d["isRepeatable"] }));
        let found = deprecated(&d["args"]);
        if !found.is_empty() {
            args.insert(format!("@{name}"), Value::Array(found));
        }
    }
    Ok(json!({
        "description": schema["description"],
        "directives": directives,
        "types": type_facts,
        "deprecatedInputFields": inputs,
        "deprecatedArgs": args,
    }))
}
