use std::collections::{HashMap, HashSet};

use apollo_compiler::Schema;
use apollo_compiler::ast::Type;
use apollo_compiler::schema::ExtendedType;
use serde_json::Value;

#[derive(Debug, Clone, Copy)]
pub(crate) enum Kind {
    Int,
    Float,
    Boolean,
    Text,
    TextOrTexts,
    Unknown,
}

fn scalar(name: &str) -> Kind {
    match name {
        "Int" => Kind::Int,
        "Float" => Kind::Float,
        "Boolean" => Kind::Boolean,
        "String" | "ID" | "Base64String" | "BigInt" | "Date" | "DateTime" | "GitObjectID"
        | "GitRefname" | "GitSSHRemote" | "GitTimestamp" | "HTML" | "PreciseDateTime" | "URI"
        | "X509Certificate" => Kind::Text,
        "CustomPropertyValue" => Kind::TextOrTexts,
        _ => Kind::Unknown,
    }
}

/// How a value of one leaf type is checked, decided once when the schema loads.
#[derive(Debug)]
pub(crate) enum Leaf {
    Enum(HashSet<String>),
    Scalar(Kind),
}

impl Leaf {
    /// Whether [`Leaf::fits`] can check this type: an enum, or a scalar whose JSON form is
    /// known.
    pub(crate) fn checkable(&self) -> bool {
        !matches!(self, Leaf::Scalar(Kind::Unknown))
    }

    /// Whether `value` is a value of the leaf type `ty`, whose named type is this leaf: `null`
    /// only where the type is nullable, a list of fitting items for a list type, an enum value
    /// of the schema's enum, and a scalar's JSON form (`BigInt` is a string, as the schema
    /// says).
    pub(crate) fn fits(&self, ty: &Type, value: &Value) -> bool {
        if value.is_null() {
            return !ty.is_non_null();
        }
        if let Type::List(item) | Type::NonNullList(item) = ty {
            return value
                .as_array()
                .is_some_and(|items| items.iter().all(|v| self.fits(item, v)));
        }
        match self {
            Leaf::Enum(values) => value.as_str().is_some_and(|v| values.contains(v)),
            Leaf::Scalar(Kind::Int) => value.is_i64(),
            Leaf::Scalar(Kind::Float) => value.is_number(),
            Leaf::Scalar(Kind::Boolean) => value.is_boolean(),
            Leaf::Scalar(Kind::Text) => value.is_string(),
            Leaf::Scalar(Kind::TextOrTexts) => {
                value.is_string()
                    || value
                        .as_array()
                        .is_some_and(|items| items.iter().all(Value::is_string))
            }
            Leaf::Scalar(Kind::Unknown) => false,
        }
    }
}

/// The [`Leaf`] of every scalar and enum type of `schema`, by name.
pub(crate) fn leaves(schema: &Schema) -> HashMap<String, Leaf> {
    schema
        .types
        .iter()
        .filter_map(|(name, ty)| {
            let leaf = match ty {
                ExtendedType::Enum(e) => {
                    Leaf::Enum(e.values.keys().map(ToString::to_string).collect())
                }
                ExtendedType::Scalar(_) => Leaf::Scalar(scalar(name)),
                _ => return None,
            };
            Some((name.to_string(), leaf))
        })
        .collect()
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
            let ty = ty(sdl);
            let leaf = schema
                .leaf(ty.inner_named_type())
                .unwrap_or_else(|| panic!("{sdl} is not a leaf"));
            assert_eq!(leaf.fits(&ty, &value), want, "{sdl} {value}");
        }
    }
}
