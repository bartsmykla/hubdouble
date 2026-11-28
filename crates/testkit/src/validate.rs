//! Response and payload validators: REST bodies against the pinned `OpenAPI` description
//! (`OpenAPI` 3.0 schemas converted to JSON Schema draft 4), webhook payloads against the pinned
//! `@octokit/webhooks-schemas` 7.6.1, with the newer fields and the `workflow_job` and
//! `check_suite` conclusions that GitHub's own description holds.

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

pub(crate) struct OpenApi {
    pub(crate) doc: Value,
    registry: Registry<'static>,
}

pub(crate) static OPENAPI: LazyLock<OpenApi> = LazyLock::new(|| {
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
        ("app", "client_id", json!({ "type": "string" })),
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
        (
            "repository$transferred/properties/changes/properties/owner/properties/from",
            "organization",
            json!({ "$ref": "#/definitions/organization" }),
        ),
    ];
    let access = json!({ "type": "string", "enum": ["read", "write"] });
    let permissions = [
        "artifact_metadata",
        "attestations",
        "code_quality",
        "copilot_requests",
        "drives",
        "models",
    ]
    .map(|name| ("app/properties/permissions", name, access.clone()));
    for (definition, field, field_schema) in newer.into_iter().chain(permissions) {
        if let Some(props) = schema
            .pointer_mut(&format!("/definitions/{definition}/properties"))
            .and_then(Value::as_object_mut)
        {
            props.insert(field.to_owned(), field_schema);
        }
    }
    schema
}

fn allow_github_conclusions(mut schema: Value) -> Value {
    let job = "/components/schemas/webhook-workflow-job-completed/properties/workflow_job/allOf";
    let suite = |action: &str| {
        (
            format!(
                "/components/schemas/webhook-check-suite-{action}/properties/check_suite/properties/conclusion"
            ),
            format!(
                "/definitions/check_suite${action}/properties/check_suite/properties/conclusion"
            ),
        )
    };
    let run_suite = |action: &str| {
        (
            "/components/schemas/simple-check-suite/properties/conclusion".to_owned(),
            format!(
                "/definitions/check_run${action}/properties/check_run/properties/check_suite/properties/conclusion"
            ),
        )
    };
    for (github, octokit) in [
        (
            format!("{job}/0/properties/conclusion"),
            "/definitions/workflow-job/properties/conclusion".to_owned(),
        ),
        (
            format!("{job}/1/properties/conclusion"),
            "/definitions/workflow_job$completed/properties/workflow_job/allOf/1/properties/conclusion".to_owned(),
        ),
        suite("completed"),
        suite("requested"),
        suite("rerequested"),
        run_suite("completed"),
        run_suite("created"),
        run_suite("requested_action"),
        run_suite("rerequested"),
    ] {
        let property = OPENAPI
            .doc
            .pointer(&github)
            .unwrap_or_else(|| panic!("the OpenAPI bundle has no {github}"));
        let mut values = property
            .get("enum")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_else(|| panic!("{github} has no enum"));
        if property.get("nullable") == Some(&Value::Bool(true)) && !values.contains(&Value::Null) {
            values.push(Value::Null);
        }
        *schema
            .pointer_mut(&format!("{octokit}/enum"))
            .unwrap_or_else(|| panic!("octokit has no {octokit}/enum")) = Value::Array(values);
    }
    schema
}

static OCTOKIT_REGISTRY: LazyLock<(Value, Registry<'static>)> = LazyLock::new(|| {
    let schema: Value = allow_github_conclusions(allow_newer_fields(
        serde_json::from_str(OCTOKIT).unwrap_or_else(|e| panic!("parse the octokit schema: {e}")),
    ));
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

const ONE_OF_READ_AS_ANY_OF: [(&str, u16); 1] = [("git/create-blob", 422)];

pub(crate) fn read_around_description_defects(
    operation_id: &str,
    status: u16,
    schema: &Value,
) -> Value {
    let mut schema = schema.clone();
    if ONE_OF_READ_AS_ANY_OF.contains(&(operation_id, status))
        && let Some(object) = schema.as_object_mut()
        && let Some(one_of) = object.remove("oneOf")
    {
        object.insert("anyOf".to_owned(), one_of);
    }
    if (operation_id, status) == ("repos/get-branch-rules", 200)
        && let Some(items) = schema.get_mut("items")
    {
        let repository_rule = json!({"allOf": [
            {"type": "object", "required": ["type"], "properties": {
                "type": {"type": "string", "enum": ["repository_create", "repository_delete",
                                                    "repository_transfer",
                                                    "repository_visibility"]},
                "parameters": {"type": "object"}}},
            {"$ref": "#/components/schemas/repository-rule-ruleset-info"}]});
        *items = json!({"anyOf": [items.take(), repository_rule]});
    }
    schema
}

pub(crate) fn resolve<'a>(doc: &'a Value, v: &'a Value) -> &'a Value {
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
    Ok(convert(read_around_description_defects(
        operation_id,
        status,
        schema,
    )))
}

pub(crate) fn openapi_schema(
    operation_id: &str,
    status: u16,
    schema: &Value,
    body: &Value,
) -> Result<(), Vec<String>> {
    check(
        &convert(read_around_description_defects(
            operation_id,
            status,
            schema,
        )),
        &OPENAPI.registry,
        Draft::Draft4,
        body,
    )
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

/// Checks one webhook delivery: its payload against octokit's schema for the `X-GitHub-Event`
/// header and the payload's `action`, and its headers against
/// <https://docs.github.com/en/webhooks/webhook-events-and-payloads#delivery-headers>:
/// `X-GitHub-Event`, `X-GitHub-Delivery` (a lowercase UUID),
/// `X-GitHub-Hook-ID` and `X-GitHub-Hook-Installation-Target-ID` (numbers),
/// `X-GitHub-Hook-Installation-Target-Type`, `X-Hub-Signature-256` (`sha256=` and 64 hex) and
/// `X-Hub-Signature` (`sha1=` and 40 hex), both or neither, a `User-Agent` that starts with
/// `GitHub-Hookshot/`, and `Content-Type: application/json`.
///
/// # Errors
///
/// Each way the delivery differs from GitHub's docs.
pub fn webhook_delivery(headers: &http::HeaderMap, body: &[u8]) -> Result<(), Vec<String>> {
    checked_webhook_delivery(headers, body).map(|_| ())
}

pub(crate) fn checked_webhook_delivery(
    headers: &http::HeaderMap,
    body: &[u8],
) -> Result<Value, Vec<String>> {
    let header = |name: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
    };
    let hex = |s: &str, len: usize| {
        s.len() == len
            && s.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    };
    let mut errors = Vec::new();
    let event = header("x-github-event");
    if event.is_empty() {
        errors.push("no X-GitHub-Event".to_owned());
    }
    let guid: Vec<&str> = header("x-github-delivery").split('-').collect();
    let lengths: Vec<usize> = guid.iter().map(|g| g.len()).collect();
    if lengths != [8, 4, 4, 4, 12] || !guid.iter().all(|g| hex(g, g.len())) {
        errors.push(format!(
            "X-GitHub-Delivery {:?} is not a lowercase UUID",
            header("x-github-delivery")
        ));
    }
    for name in ["x-github-hook-id", "x-github-hook-installation-target-id"] {
        if header(name).parse::<u64>().is_err() {
            errors.push(format!("{name} {:?} is not a number", header(name)));
        }
    }
    if header("x-github-hook-installation-target-type").is_empty() {
        errors.push("no X-GitHub-Hook-Installation-Target-Type".to_owned());
    }
    let sha256 = header("x-hub-signature-256");
    let sha1 = header("x-hub-signature");
    match (sha256.is_empty(), sha1.is_empty()) {
        (true, true) => {}
        (false, false) => {
            if !sha256.strip_prefix("sha256=").is_some_and(|h| hex(h, 64)) {
                errors.push(format!("X-Hub-Signature-256 {sha256:?}"));
            }
            if !sha1.strip_prefix("sha1=").is_some_and(|h| hex(h, 40)) {
                errors.push(format!("X-Hub-Signature {sha1:?}"));
            }
        }
        _ => errors.push("only one of X-Hub-Signature and X-Hub-Signature-256".to_owned()),
    }
    if !header("user-agent").starts_with("GitHub-Hookshot/") {
        errors.push(format!("User-Agent {:?}", header("user-agent")));
    }
    if header("content-type").split(';').next().map(str::trim) != Some("application/json") {
        errors.push(format!("Content-Type {:?}", header("content-type")));
    }
    let payload = match serde_json::from_slice::<Value>(body) {
        Ok(payload) => {
            let action = payload.get("action").and_then(Value::as_str);
            if let Err(e) = webhook_payload(event, action, &payload) {
                errors.extend(e);
            }
            payload
        }
        Err(e) => {
            errors.push(format!("the payload is not JSON: {e}"));
            Value::Null
        }
    };
    if errors.is_empty() {
        Ok(payload)
    } else {
        Err(errors)
    }
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
    fn only_the_listed_description_defect_reads_one_of_as_any_of() {
        // Given a blob validation error as GitHub sends it, and the description's own 422 schema
        let error = json!({
            "message": "Validation Failed",
            "documentation_url": "https://docs.github.com/rest/git/blobs#create-a-blob",
            "status": "422",
            "errors": [{"resource": "Blob", "field": "encoding", "code": "invalid"}]
        });
        let described = OPENAPI
            .doc
            .pointer("/paths/~1repos~1{owner}~1{repo}~1git~1blobs/post/responses/422/content/application~1json/schema")
            .unwrap()
            .clone();
        // When
        let as_described = check(
            &convert(described.clone()),
            &OPENAPI.registry,
            Draft::Draft4,
            &error,
        );
        let read_around = openapi_response("git/create-blob", 422, &error);
        let elsewhere = openapi_schema("orgs/update", 422, &described, &error);
        let mut broken = error;
        broken["message"] = json!(7);
        let still_checked = openapi_response("git/create-blob", 422, &broken);
        // Then
        assert!(as_described.is_err());
        assert_eq!(read_around, Ok(()));
        assert!(elsewhere.is_err());
        assert!(still_checked.is_err());
    }

    #[test]
    fn an_unknown_operation_or_event_has_no_schema() {
        assert!(openapi_response("nope/nope", 200, &json!({})).is_err());
        assert!(webhook_payload("nope", None, &json!({})).is_err());
    }

    #[test]
    fn a_delivery_needs_githubs_headers_and_a_schema_valid_payload() {
        // Given a ping as GitHub delivers it
        let ping = br#"{"zen": "z", "hook_id": 1, "hook": {"type": "App", "id": 1, "name": "web",
            "active": true, "events": ["push"], "config": {"content_type": "json",
            "insecure_ssl": "0", "url": "http://r/webhook"}, "updated_at": "2026-01-01T00:00:00Z",
            "created_at": "2026-01-01T00:00:00Z", "app_id": 1, "deliveries_url": "http://d",
            "url": "http://h", "ping_url": "http://p"}}"#;
        let mut headers = http::HeaderMap::new();
        for (n, v) in [
            ("x-github-event", "ping"),
            ("x-github-delivery", "72d3162e-cc78-11e3-81ab-4c9367dc0958"),
            ("x-github-hook-id", "292430182"),
            ("x-github-hook-installation-target-id", "79929171"),
            ("x-github-hook-installation-target-type", "integration"),
            (
                "x-hub-signature",
                "sha1=7d38cdd689735b008b3c702edd92eea23791c5f6",
            ),
            (
                "x-hub-signature-256",
                "sha256=d57c68ca6f92289e6987922ff26938930f6e66a2d161ef06abdf1859230aa23c",
            ),
            ("user-agent", "GitHub-Hookshot/044aadd"),
            ("content-type", "application/json"),
        ] {
            headers.insert(n, v.parse().unwrap());
        }
        // Then
        assert_eq!(webhook_delivery(&headers, ping), Ok(()));
        for (name, bad) in [
            ("x-github-delivery", "72D3162E-CC78-11E3-81AB-4C9367DC0958"),
            ("x-github-hook-id", "web"),
            ("x-hub-signature-256", "sha256=nope"),
            ("user-agent", "hubdouble"),
            ("content-type", "application/x-www-form-urlencoded"),
        ] {
            let mut broken = headers.clone();
            broken.insert(name, bad.parse().unwrap());
            assert!(webhook_delivery(&broken, ping).is_err(), "{name}");
        }
        let mut unsigned = headers.clone();
        unsigned.remove("x-hub-signature");
        assert!(webhook_delivery(&unsigned, ping).is_err());
        assert!(webhook_delivery(&headers, br#"{"zen": 1}"#).is_err());
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
