use apollo_compiler::ExecutableDocument;
use apollo_compiler::introspection::partial_execute;
use apollo_compiler::request::coerce_variable_values;
use apollo_compiler::response::{JsonMap, JsonValue};
use bytes::Bytes;
use serde::{Deserialize, Serialize};

use crate::contract::{ErrorType, GqlError};
use crate::schema::Schema;

const QUERY: &str = "query IntrospectionQuery {
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

#[derive(Serialize)]
struct Answer<'a> {
    data: &'a JsonMap,
}

fn internal(e: impl std::fmt::Debug) -> GqlError {
    GqlError::new(ErrorType::Internal, format!("{e:?}"))
}

#[derive(Deserialize)]
struct BuiltIns {
    types: Vec<JsonValue>,
    directives: Vec<JsonValue>,
}

fn name(v: &JsonValue) -> Option<&str> {
    v.get("name").and_then(JsonValue::as_str)
}

fn sort_by_name(items: &mut [JsonValue]) {
    items.sort_by_cached_key(|v| name(v).map(str::to_owned));
}

fn merge(meta: &mut JsonMap, built_ins: BuiltIns) {
    if let Some(JsonValue::Array(types)) = meta.get_mut("types") {
        types.retain(|t| !built_ins.types.iter().any(|b| name(b) == name(t)));
        types.extend(built_ins.types);
        sort_by_name(types);
        for t in types {
            if let Some(JsonValue::Array(fields)) = t.get_mut("fields") {
                sort_by_name(fields);
            }
        }
    }
    meta.insert("directives", JsonValue::Array(built_ins.directives));
}

pub(crate) fn build(schema: &Schema) -> Result<Bytes, GqlError> {
    let inner = &schema.inner;
    let doc = ExecutableDocument::parse_and_validate(inner, QUERY, "introspection.graphql")
        .map_err(|e| internal(e.errors))?;
    let operation = doc.operations.get(None).map_err(internal)?;
    let variables = coerce_variable_values(inner, operation, &JsonMap::new()).map_err(internal)?;
    let response = partial_execute(
        inner,
        &inner.implementers_map(),
        &doc,
        operation,
        &variables,
    )
    .map_err(internal)?;
    let mut data = response
        .data
        .ok_or_else(|| internal("the introspection query answered no data"))?;
    let built_ins: BuiltIns =
        serde_json::from_str(hubdouble_spec::GRAPHQL_INTROSPECTION_BUILT_INS).map_err(internal)?;
    if let Some(JsonValue::Object(meta)) = data.get_mut("__schema") {
        merge(meta, built_ins);
    }
    serde_json::to_vec(&Answer { data: &data })
        .map(Bytes::from)
        .map_err(internal)
}
