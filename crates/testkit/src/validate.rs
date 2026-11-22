//! Response and payload validators: REST bodies against the pinned `OpenAPI` description
//! (`OpenAPI` 3.0 schemas converted to JSON Schema draft 4), webhook payloads against the pinned
//! `@octokit/webhooks-schemas` 7.6.1.

use std::sync::LazyLock;

use jsonschema::{Draft, Registry};
use serde_json::{Map, Value, json};

const OPENAPI_URI: &str = "https://hubdouble.test/openapi.json";
const OCTOKIT_URI: &str = "https://hubdouble.test/octokit.json";
const OPENAPI_PATH: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../spec/openapi/api.github.com.json"
);
const OCTOKIT: &str = include_str!("../spec/octokit/schema.json");

struct OpenApi {
    doc: Value,
    registry: Registry<'static>,
}

static OPENAPI: LazyLock<OpenApi> = LazyLock::new(|| {
    let text = std::fs::read_to_string(OPENAPI_PATH)
        .unwrap_or_else(|e| panic!("read {OPENAPI_PATH}: {e}"));
    let doc: Value =
        serde_json::from_str(&text).unwrap_or_else(|e| panic!("parse the OpenAPI bundle: {e}"));
    let components =
        json!({ "components": convert(doc.get("components").cloned().unwrap_or_default()) });
    let registry = Registry::new()
        .draft(Draft::Draft4)
        .retrieve_known_draft_meta_schemas(false)
        .add(OPENAPI_URI, components)
        .and_then(jsonschema::RegistryBuilder::prepare)
        .unwrap_or_else(|e| panic!("index the OpenAPI schemas: {e}"));
    OpenApi { doc, registry }
});

fn allow_newer_fields(mut schema: Value) -> Value {
    let newer = [
        ("user", "user_view_type", json!({ "type": "string" })),
        ("release", "immutable", json!({ "type": "boolean" })),
        (
            "release",
            "updated_at",
            json!({ "type": ["string", "null"] }),
        ),
        (
            "release-asset",
            "digest",
            json!({ "type": ["string", "null"] }),
        ),
    ];
    for (definition, field, field_schema) in newer {
        if let Some(props) = schema
            .pointer_mut(&format!("/definitions/{definition}/properties"))
            .and_then(Value::as_object_mut)
        {
            props.insert(field.to_owned(), field_schema);
        }
    }
    schema
}

static OCTOKIT_REGISTRY: LazyLock<(Value, Registry<'static>)> = LazyLock::new(|| {
    let schema: Value = allow_newer_fields(
        serde_json::from_str(OCTOKIT).unwrap_or_else(|e| panic!("parse the octokit schema: {e}")),
    );
    let registry = Registry::new()
        .draft(Draft::Draft7)
        .retrieve_known_draft_meta_schemas(false)
        .add(OCTOKIT_URI, schema.clone())
        .and_then(jsonschema::RegistryBuilder::prepare)
        .unwrap_or_else(|e| panic!("index the octokit schema: {e}"));
    (schema, registry)
});

fn convert(v: Value) -> Value {
    match v {
        Value::Object(map) => {
            let nullable = map.get("nullable") == Some(&Value::Bool(true));
            let mut out = Map::with_capacity(map.len());
            for (k, v) in map {
                match (k.as_str(), v) {
                    ("nullable", Value::Bool(_)) => {}
                    ("$ref", Value::String(r)) if r.starts_with("#/") => {
                        out.insert(k, Value::String(format!("{OPENAPI_URI}{r}")));
                    }
                    (_, v) => {
                        out.insert(k, convert(v));
                    }
                }
            }
            if nullable {
                json!({ "anyOf": [Value::Object(out), { "type": "null" }] })
            } else {
                Value::Object(out)
            }
        }
        Value::Array(items) => Value::Array(items.into_iter().map(convert).collect()),
        other => other,
    }
}

fn resolve<'a>(doc: &'a Value, v: &'a Value) -> &'a Value {
    match v
        .get("$ref")
        .and_then(Value::as_str)
        .and_then(|r| r.strip_prefix('#'))
    {
        Some(pointer) => doc.pointer(pointer).unwrap_or(v),
        None => v,
    }
}

fn check(
    schema: &Value,
    registry: &Registry<'static>,
    draft: Draft,
    instance: &Value,
) -> Result<(), Vec<String>> {
    let validator = jsonschema::options()
        .with_draft(draft)
        .with_registry(registry)
        .build(schema)
        .map_err(|e| vec![format!("the schema does not compile: {e}")])?;
    let errors: Vec<String> = validator
        .iter_errors(instance)
        .map(|e| format!("{}: {e}", e.instance_path()))
        .collect();
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

/// The `application/json` schema of `operation_id`'s `status` answer, converted.
///
/// # Errors
///
/// When the operation, the status (or `default`) or its JSON schema is not in the description.
pub fn response_schema(operation_id: &str, status: u16) -> Result<Value, String> {
    let api = &*OPENAPI;
    let op = api
        .doc
        .get("paths")
        .and_then(Value::as_object)
        .into_iter()
        .flat_map(|paths| paths.values())
        .filter_map(Value::as_object)
        .flat_map(|methods| methods.values())
        .find(|op| op.get("operationId").and_then(Value::as_str) == Some(operation_id))
        .ok_or_else(|| format!("no operation {operation_id}"))?;
    let responses = op
        .get("responses")
        .ok_or_else(|| format!("{operation_id} has no responses"))?;
    let response = responses
        .get(status.to_string())
        .or_else(|| responses.get("default"))
        .ok_or_else(|| format!("{operation_id} documents no {status} answer"))?;
    let schema = resolve(&api.doc, response)
        .pointer("/content/application~1json/schema")
        .ok_or_else(|| format!("{operation_id} {status} has no JSON schema"))?;
    Ok(convert(schema.clone()))
}

/// Validates a REST answer body against GitHub's schema for `operation_id` and `status`.
///
/// # Errors
///
/// Each violation with its instance path, or why there is no schema.
pub fn openapi_response(operation_id: &str, status: u16, body: &Value) -> Result<(), Vec<String>> {
    let schema = response_schema(operation_id, status).map_err(|e| vec![e])?;
    check(&schema, &OPENAPI.registry, Draft::Draft4, body)
}

/// Validates a webhook payload against octokit's schema for `event` and `action`
/// (`issue_comment$created`, or `push$event` for an event without actions).
///
/// # Errors
///
/// Each violation with its instance path, or why there is no schema.
pub fn webhook_payload(
    event: &str,
    action: Option<&str>,
    payload: &Value,
) -> Result<(), Vec<String>> {
    let (schema, registry) = &*OCTOKIT_REGISTRY;
    let name = format!("{event}${}", action.unwrap_or("event"));
    if schema
        .pointer(&format!(
            "/definitions/{}",
            name.replace('~', "~0").replace('/', "~1")
        ))
        .is_none()
    {
        return Err(vec![format!("octokit has no schema {name}")]);
    }
    let target = json!({ "$ref": format!("{OCTOKIT_URI}#/definitions/{name}") });
    check(&target, registry, Draft::Draft7, payload)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nullable_schemas_accept_null_and_refs_become_absolute() {
        let s = convert(json!({"type": "object", "properties": {
            "a": {"type": "string", "nullable": true},
            "b": {"$ref": "#/components/schemas/simple-user"}
        }}));
        assert_eq!(
            s["properties"]["a"],
            json!({"anyOf": [{"type": "string"}, {"type": "null"}]})
        );
        assert_eq!(
            s["properties"]["b"]["$ref"],
            format!("{OPENAPI_URI}#/components/schemas/simple-user")
        );
    }

    #[test]
    fn a_github_answer_passes_and_a_broken_one_fails() {
        // Given
        let zen = response_schema("meta/root", 200).unwrap();
        assert!(zen.is_object());
        let user = json!({
            "login": "octocat", "id": 1, "node_id": "MDQ6VXNlcjE=", "avatar_url": "https://a",
            "gravatar_id": "", "url": "https://u", "html_url": "https://h", "followers_url": "https://f",
            "following_url": "https://f", "gists_url": "https://g", "starred_url": "https://s",
            "subscriptions_url": "https://s", "organizations_url": "https://o", "repos_url": "https://r",
            "events_url": "https://e", "received_events_url": "https://r", "type": "User",
            "site_admin": false, "name": null, "email": null
        });
        // When
        let ok = openapi_response("orgs/list-members", 200, &json!([user.clone()]));
        let mut broken = user;
        broken["id"] = json!("one");
        let bad = openapi_response("orgs/list-members", 200, &json!([broken]));
        // Then
        assert_eq!(ok, Ok(()));
        let errors = bad.unwrap_err();
        assert!(errors.iter().any(|e| e.starts_with("/0/id")), "{errors:?}");
    }

    #[test]
    fn an_unknown_operation_or_event_has_no_schema() {
        assert!(openapi_response("nope/nope", 200, &json!({})).is_err());
        assert!(webhook_payload("nope", None, &json!({})).is_err());
    }

    #[test]
    fn a_ping_payload_is_checked_against_octokit() {
        let ping = json!({
            "zen": "Keep it logically awesome.", "hook_id": 1,
            "hook": {"type": "App", "id": 1, "name": "web", "active": true, "events": ["push"],
                     "config": {"content_type": "json", "insecure_ssl": "0", "url": "http://r/webhook"},
                     "updated_at": "2026-01-01T00:00:00Z", "created_at": "2026-01-01T00:00:00Z",
                     "app_id": 1, "deliveries_url": "http://d", "url": "http://h",
                     "ping_url": "http://p"}
        });
        assert_eq!(webhook_payload("ping", None, &ping), Ok(()));
        let errors = webhook_payload("ping", None, &json!({"zen": 1})).unwrap_err();
        assert_ne!(errors, Vec::<String>::new());
    }
}
