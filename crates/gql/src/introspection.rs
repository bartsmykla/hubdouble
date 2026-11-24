use apollo_compiler::ExecutableDocument;
use apollo_compiler::introspection::partial_execute;
use apollo_compiler::request::coerce_variable_values;
use apollo_compiler::response::JsonMap;
use serde_json::Value;

use crate::pin::is_built_in;
use crate::schema::Schema;

pub(crate) const CLASSIC_QUERY: &str = "query IntrospectionQuery {
  __schema {
    queryType { name }
    mutationType { name }
    subscriptionType { name }
    types { ...FullType }
    directives { name description locations args { ...InputValue } }
  }
}
fragment FullType on __Type {
  kind
  name
  description
  fields(includeDeprecated: true) {
    name
    description
    args { ...InputValue }
    type { ...TypeRef }
    isDeprecated
    deprecationReason
  }
  inputFields { ...InputValue }
  interfaces { ...TypeRef }
  enumValues(includeDeprecated: true) { name description isDeprecated deprecationReason }
  possibleTypes { ...TypeRef }
}
fragment InputValue on __InputValue { name description type { ...TypeRef } defaultValue }
fragment TypeRef on __Type {
  kind name ofType { kind name ofType { kind name ofType { kind name ofType {
    kind name ofType { kind name ofType { kind name ofType { kind name } } }
  } } } }
}";

fn user_types(schema: &Value) -> Vec<Value> {
    schema["types"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|t| !is_built_in(t["name"].as_str().unwrap()))
        .cloned()
        .collect()
}

#[test]
fn the_served_schema_is_the_sdl_of_the_pins() {
    // Given the two introspection pins
    let introspection = hubdouble_spec::GRAPHQL_INTROSPECTION;
    let extra = hubdouble_spec::GRAPHQL_INTROSPECTION_EXTRA;

    // When the SDL is written from them
    let sdl = crate::schema_sdl(introspection, extra).unwrap();

    // Then it is the embedded schema, so `mise run gen` left nothing to write
    assert!(
        sdl == hubdouble_spec::GRAPHQL_SCHEMA,
        "spec/graphql/schema.graphql differs from its pins: run `mise run gen`"
    );
}

#[test]
fn the_served_schema_describes_every_pinned_type_as_live_github_does() {
    // Given the served schema and the pinned live answer
    let schema = Schema::load().unwrap();
    let inner = &schema.inner;
    let pin: Value = serde_json::from_str(hubdouble_spec::GRAPHQL_INTROSPECTION).unwrap();

    // When the live query runs on the served schema
    let doc = ExecutableDocument::parse_and_validate(inner, CLASSIC_QUERY, "q.graphql").unwrap();
    let operation = doc.operations.get(None).unwrap();
    let variables = coerce_variable_values(inner, operation, &JsonMap::new()).unwrap();
    let answer = partial_execute(
        inner,
        &inner.implementers_map(),
        &doc,
        operation,
        &variables,
    )
    .expect("the introspection query runs");
    let answer = serde_json::to_value(answer.data.unwrap()).unwrap();

    // Then every type outside the built-ins is the pinned one, in the pinned order
    let mut served = user_types(&answer["__schema"]);
    served.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    assert_eq!(served, user_types(&pin["data"]["__schema"]));
}
