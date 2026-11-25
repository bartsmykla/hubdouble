use std::sync::LazyLock;

use hubdouble_gql::Schema;
use serde_json::{Value, json};

static SCHEMA: LazyLock<Schema> = LazyLock::new(|| {
    Schema::parse(
        "
    enum State { OPEN CLOSED }
    interface Item { id: ID! }
    type Entry implements Item { id: ID!, state: State!, tags: [String!]! }
    type Note implements Item { id: ID!, message: String! }
    type Query { item: Item, list: [Entry!], required: Entry!, formats: Formats }
    scalar DateTime
    scalar GitTimestamp
    scalar URI
    scalar HTML
    scalar GitObjectID
    scalar GitSSHRemote
    scalar Base64String
    scalar BigInt
    scalar X509Certificate
    scalar PreciseDateTime
    type Formats {
        date: DateTime, gitDate: GitTimestamp, url: URI, html: HTML, hash: GitObjectID,
        ssh: GitSSHRemote, bytes: Base64String, count: BigInt, cert: X509Certificate,
        precise: PreciseDateTime, number: Int
    }
",
    )
    .unwrap_or_else(|e| panic!("parse answer-check test schema: {e}"))
});

#[test]
fn an_int_answer_may_pass_32_bits_but_not_be_a_fraction() {
    // Given an Int field, which live cli/cli answered as databaseId 139044377974 on 2026-11-25
    let doc = "{ formats { number } }";
    let answer = |n: Value| json!({"data": {"formats": {"number": n}}});
    // Then a wide integer is GitHub's answer, and a fraction is not
    assert!(
        SCHEMA
            .check_answer(doc, &Value::Null, &answer(json!(139_044_377_974_i64)))
            .is_ok()
    );
    assert!(
        SCHEMA
            .check_answer(doc, &Value::Null, &answer(json!(1.5)))
            .is_err()
    );
}

#[test]
fn rejects_wrong_fields_types_and_fragment_branches() {
    let doc = "query($yes: Boolean! = false) { sample: item {
        actual: __typename ...Identity ... on Entry { state tags @include(if:$yes) }
        ... on Note { message }
    }} fragment Identity on Item { key: id }";
    let correct = json!({"data":{"sample":{"actual":"Entry","key":"a","state":"OPEN"}}});
    assert!(SCHEMA.check_answer(doc, &Value::Null, &correct).is_ok());
    for (field, value) in [
        ("actual", json!("Other")),
        ("actual", json!("Note")),
        ("key", json!(9)),
        ("state", json!("BROKEN")),
        ("state", Value::Null),
        ("message", json!("extra")),
        ("tags", json!(["extra"])),
    ] {
        let mut broken = correct.clone();
        broken["data"]["sample"][field] = value;
        assert!(
            SCHEMA.check_answer(doc, &Value::Null, &broken).is_err(),
            "{broken}"
        );
    }
    let mut missing = correct.clone();
    missing["data"]["sample"]
        .as_object_mut()
        .unwrap()
        .remove("state");
    assert!(SCHEMA.check_answer(doc, &Value::Null, &missing).is_err());
    let vars = json!({"yes":true});
    let mut tagged = correct;
    tagged["data"]["sample"]["tags"] = json!(["ok"]);
    assert!(SCHEMA.check_answer(doc, &vars, &tagged).is_ok());
    tagged["data"]["sample"]["tags"] = json!([null]);
    assert!(SCHEMA.check_answer(doc, &vars, &tagged).is_err());
}

#[test]
fn a_bubbled_error_has_the_path_to_the_non_null_descendant() {
    let doc = "{ batch: list { labels: tags } }";
    let mut result = json!({"data":{"batch":null},"errors":[{
        "type":"FORBIDDEN","message":"Access denied","path":["batch",0,"labels",2],
        "locations":[{"line":1,"column":17}]
    }]});
    assert!(SCHEMA.check_answer(doc, &Value::Null, &result).is_ok());
    result["data"]["batch"] = json!([{"labels":["a","b",null]}]);
    assert!(SCHEMA.check_answer(doc, &Value::Null, &result).is_err());
    result["data"]["batch"] = Value::Null;
    result["errors"][0]["path"] = json!(["batch", 0, "tags", 2]);
    assert!(SCHEMA.check_answer(doc, &Value::Null, &result).is_err());
    result["errors"][0]["path"] = json!(["batch", "labels", 2]);
    assert!(SCHEMA.check_answer(doc, &Value::Null, &result).is_err());
    result["errors"][0]["path"] = json!(["batch", 0, "labels", 2]);
    result["errors"][0]["locations"][0]["line"] = json!(2);
    assert!(SCHEMA.check_answer(doc, &Value::Null, &result).is_err());
}

#[test]
fn checks_custom_scalar_formats() {
    let doc = "{ formats { date gitDate url html hash ssh bytes count cert precise } }";
    let correct = json!({"data":{"formats":{
        "date":"2024-02-29T00:00:00Z","gitDate":"2024-01-01T00:00:00+01:30",
        "url":"https://github.com/{owner}/{repo}","html":"<p>Hello</p>",
        "hash":"0123456789abcdef0123456789abcdef01234567","ssh":"git@github.com:org/repo.git",
        "bytes":"YQo=","count":"-9223372036854775809","cert":null,
        "precise":"2024-01-01T00:00:00.030Z"
    }}});
    assert!(SCHEMA.check_answer(doc, &Value::Null, &correct).is_ok());
    for (field, bad) in [
        ("date", "2025-02-29T00:00:00Z"),
        ("date", "2024-02-29T00:00:00+01:00"),
        ("gitDate", "2024-01-01T00:00:00+01:93"),
        ("url", "https://github.com/%ZZ"),
        ("html", "\0"),
        ("hash", "0123"),
        ("ssh", "a b:xyz"),
        ("bytes", "Y??="),
        ("count", "1.0"),
        (
            "cert",
            "-----BEGIN CERTIFICATE-----YQo=-----END CERTIFICATE-----",
        ),
        ("precise", "2024-01-01T00:00:00Z"),
    ] {
        let mut body = correct.clone();
        body["data"]["formats"][field] = json!(bad);
        assert!(
            SCHEMA.check_answer(doc, &Value::Null, &body).is_err(),
            "{field}"
        );
    }
}

#[test]
fn checks_errors_even_when_a_request_does_not_validate() {
    let doc = "query { item { absent } }";
    let valid = json!({"errors":[{
        "message":"Unknown field","path":["query","item","absent"],
        "locations":[{"line":1,"column":16}],"extensions":{"code":"undefinedField"}
    }]});
    assert!(SCHEMA.check_answer(doc, &Value::Null, &valid).is_ok());
    for (key, value) in [
        ("message", json!(0)),
        ("path", json!([0, "absent"])),
        ("locations", json!([{"line":0,"column":16}])),
        ("extensions", json!([])),
    ] {
        let mut body = valid.clone();
        body["errors"][0][key] = value;
        assert!(SCHEMA.check_answer(doc, &Value::Null, &body).is_err());
    }
    let mut wrong = valid;
    wrong["data"] = Value::Null;
    assert!(SCHEMA.check_answer(doc, &Value::Null, &wrong).is_err());
}
