use apollo_compiler::Schema;
use apollo_compiler::ast::Type;
use apollo_compiler::schema::ExtendedType;
use serde_json::Value;

enum Kind {
    Int,
    Float,
    Boolean,
    Text,
    TextOrTexts,
}

fn scalar(name: &str) -> Option<Kind> {
    Some(match name {
        "Int" => Kind::Int,
        "Float" => Kind::Float,
        "Boolean" => Kind::Boolean,
        "String" | "ID" | "Base64String" | "BigInt" | "Date" | "DateTime" | "GitObjectID"
        | "GitRefname" | "GitSSHRemote" | "GitTimestamp" | "HTML" | "PreciseDateTime" | "URI"
        | "X509Certificate" => Kind::Text,
        "CustomPropertyValue" => Kind::TextOrTexts,
        _ => return None,
    })
}

/// Whether the leaf type `name` is one [`fits`] can check: an enum, or a scalar whose JSON
/// form is known.
pub(crate) fn checkable(schema: &Schema, name: &str) -> bool {
    match schema.types.get(name) {
        Some(ExtendedType::Enum(_)) => true,
        Some(ExtendedType::Scalar(_)) => scalar(name).is_some(),
        _ => false,
    }
}

/// Whether `value` is a value of the leaf type `ty`: `null` only where the type is nullable,
/// a list of fitting items for a list type, an enum value of the schema's enum, and a scalar's
/// JSON form (`BigInt` is a string, as the schema says).
pub(crate) fn fits(schema: &Schema, ty: &Type, value: &Value) -> bool {
    if value.is_null() {
        return !ty.is_non_null();
    }
    if let Type::List(item) | Type::NonNullList(item) = ty {
        return value
            .as_array()
            .is_some_and(|items| items.iter().all(|v| fits(schema, item, v)));
    }
    let name = ty.inner_named_type().as_str();
    match schema.types.get(name) {
        Some(ExtendedType::Enum(e)) => value
            .as_str()
            .is_some_and(|v| e.values.keys().any(|k| k.as_str() == v)),
        Some(ExtendedType::Scalar(_)) => match scalar(name) {
            Some(Kind::Int) => value.is_i64(),
            Some(Kind::Float) => value.is_number(),
            Some(Kind::Boolean) => value.is_boolean(),
            Some(Kind::Text) => value.is_string(),
            Some(Kind::TextOrTexts) => {
                value.is_string()
                    || value
                        .as_array()
                        .is_some_and(|items| items.iter().all(Value::is_string))
            }
            None => false,
        },
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ty(sdl: &str) -> Type {
        Type::parse(sdl, "type.graphql").unwrap_or_else(|e| panic!("{e}"))
    }

    #[test]
    fn a_leaf_fits_only_its_schema_type() {
        let schema = crate::Schema::load().unwrap_or_else(|e| panic!("{e}"));
        let schema = &schema.inner;
        for (sdl, value, want) in [
            ("TeamPrivacy!", json!("SECRET"), true),
            ("TeamPrivacy!", json!("CLOSED"), false),
            ("LockReason", json!("OFF_TOPIC"), true),
            ("LockReason", json!("off-topic"), false),
            ("LockReason", json!(null), true),
            ("ID!", json!("PRR_kwDO"), true),
            ("ID!", json!(42), false),
            ("Int!", json!(null), false),
            ("Int!", json!(3), true),
            ("Int", json!("3"), false),
            ("Float", json!(1.5), true),
            ("Boolean!", json!(true), true),
            ("DateTime!", json!("2026-01-01T00:00:00Z"), true),
            ("BigInt", json!(12), false),
            ("[String!]!", json!(["a", "b"]), true),
            ("[String!]!", json!(["a", null]), false),
            ("CustomPropertyValue", json!(["a"]), true),
        ] {
            assert_eq!(fits(schema, &ty(sdl), &value), want, "{sdl} {value}");
        }
    }
}
