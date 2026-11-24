use std::collections::HashMap;

use serde_json::{Map, Value};

use crate::contract::GqlError;
use crate::pin::{invalid, with_deprecated};

const OPERATION: [&str; 3] = ["QUERY", "MUTATION", "SUBSCRIPTION"];
const FRAGMENT: [&str; 3] = ["FRAGMENT_DEFINITION", "FRAGMENT_SPREAD", "INLINE_FRAGMENT"];

/// GitHub's introspection data, read from the two pins, that `__schema` and `__type` answer.
#[derive(Debug)]
pub(crate) struct Meta {
    schema: Value,
    extra: Value,
    by_name: HashMap<String, usize>,
}

#[derive(Clone, Copy)]
pub(crate) enum M<'a> {
    Schema,
    Named(&'a Value),
    Wrap(&'a Value),
    Field(&'a str, &'a Value),
    Input(&'a Value),
    Enum(&'a Value),
    Directive(&'a Value),
}

pub(crate) enum R<'a> {
    Leaf(Value),
    One(Option<M<'a>>),
    Many(Option<Vec<M<'a>>>),
}

impl M<'_> {
    pub(crate) fn type_name(self) -> &'static str {
        match self {
            M::Schema => "__Schema",
            M::Named(_) | M::Wrap(_) => "__Type",
            M::Field(..) => "__Field",
            M::Input(_) => "__InputValue",
            M::Enum(_) => "__EnumValue",
            M::Directive(_) => "__Directive",
        }
    }
}

fn include_deprecated(args: &Map<String, Value>) -> bool {
    args.get("includeDeprecated") == Some(&Value::Bool(true))
}

fn kept<'a>(items: Vec<&'a Value>, args: &Map<String, Value>) -> Vec<&'a Value> {
    let all = include_deprecated(args);
    items
        .into_iter()
        .filter(|v| all || v["isDeprecated"] != true)
        .collect()
}

fn copy(v: &Value, key: &str) -> R<'static> {
    R::Leaf(v.get(key).cloned().unwrap_or(Value::Null))
}

fn any_location(d: &Value, names: &[&str]) -> R<'static> {
    let on = d["locations"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|l| l.as_str().is_some_and(|l| names.contains(&l)));
    R::Leaf(Value::Bool(on))
}

impl Meta {
    pub(crate) fn load(introspection: &str, extra: &str) -> Result<Meta, GqlError> {
        let mut answer: Value =
            serde_json::from_str(introspection).map_err(|_| invalid("no JSON answer"))?;
        let extra = serde_json::from_str(extra).map_err(|_| invalid("no JSON extra facts"))?;
        let schema = answer["data"]["__schema"].take();
        let by_name = schema["types"]
            .as_array()
            .ok_or_else(|| invalid("no types"))?
            .iter()
            .enumerate()
            .filter_map(|(i, t)| Some((t["name"].as_str()?.to_owned(), i)))
            .collect();
        Ok(Meta {
            schema,
            extra,
            by_name,
        })
    }

    fn named(&self, name: &str) -> Option<&Value> {
        self.by_name
            .get(name)
            .and_then(|i| self.schema["types"].get(*i))
    }

    pub(crate) fn type_by_name(&self, name: &str) -> Option<M<'_>> {
        self.named(name).map(M::Named)
    }

    fn type_ref<'a>(&'a self, r: &'a Value) -> Option<M<'a>> {
        match r["name"].as_str() {
            Some(name) => self.type_by_name(name),
            None if r.is_object() => Some(M::Wrap(r)),
            None => None,
        }
    }

    fn refs<'a>(&'a self, list: &'a Value) -> R<'a> {
        R::Many(
            list.as_array()
                .map(|l| l.iter().filter_map(|r| self.type_ref(r)).collect()),
        )
    }

    fn inputs<'a>(&'a self, listed: &'a Value, key: &str, args: &Map<String, Value>) -> R<'a> {
        let all = with_deprecated(listed, &self.extra["deprecatedArgs"][key]);
        R::Many(Some(kept(all, args).into_iter().map(M::Input).collect()))
    }

    pub(crate) fn get<'a>(&'a self, m: M<'a>, field: &str, args: &Map<String, Value>) -> R<'a> {
        match m {
            M::Schema => match field {
                "description" => R::Leaf(self.extra["description"].clone()),
                "types" => self.refs(&self.schema["types"]),
                "queryType" | "mutationType" | "subscriptionType" => {
                    R::One(self.type_ref(&self.schema[field]))
                }
                "directives" => R::Many(Some(
                    self.schema["directives"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .map(M::Directive)
                        .collect(),
                )),
                _ => R::Leaf(Value::Null),
            },
            M::Named(t) => self.named_field(t, field, args),
            M::Wrap(r) => match field {
                "kind" => copy(r, "kind"),
                "ofType" => R::One(self.type_ref(&r["ofType"])),
                "isOneOf" => R::Leaf(Value::Bool(false)),
                _ => R::Leaf(Value::Null),
            },
            M::Field(owner, f) => match field {
                "args" => {
                    let name = f["name"].as_str().unwrap_or_default();
                    self.inputs(&f["args"], &format!("{owner}.{name}"), args)
                }
                "type" => R::One(self.type_ref(&f["type"])),
                _ => copy(f, field),
            },
            M::Input(v) => match field {
                "type" => R::One(self.type_ref(&v["type"])),
                "isDeprecated" => R::Leaf(Value::Bool(v["isDeprecated"] == true)),
                _ => copy(v, field),
            },
            M::Enum(v) => copy(v, field),
            M::Directive(d) => match field {
                "args" => {
                    let name = d["name"].as_str().unwrap_or_default();
                    self.inputs(&d["args"], &format!("@{name}"), args)
                }
                "isRepeatable" => {
                    let name = d["name"].as_str().unwrap_or_default();
                    R::Leaf(Value::Bool(
                        self.extra["directives"][name]["isRepeatable"] == true,
                    ))
                }
                "onOperation" => any_location(d, &OPERATION),
                "onFragment" => any_location(d, &FRAGMENT),
                "onField" => any_location(d, &["FIELD"]),
                _ => copy(d, field),
            },
        }
    }

    fn named_field<'a>(&'a self, t: &'a Value, field: &str, args: &Map<String, Value>) -> R<'a> {
        let name = t["name"].as_str().unwrap_or_default();
        let facts = &self.extra["types"][name];
        match field {
            "fields" => R::Many(t["fields"].as_array().map(|l| {
                kept(l.iter().collect(), args)
                    .into_iter()
                    .map(|f| M::Field(name, f))
                    .collect()
            })),
            "interfaces" | "possibleTypes" => self.refs(&t[field]),
            "enumValues" => R::Many(t["enumValues"].as_array().map(|l| {
                kept(l.iter().collect(), args)
                    .into_iter()
                    .map(M::Enum)
                    .collect()
            })),
            "inputFields" => {
                if t["inputFields"].is_null() {
                    return R::Many(None);
                }
                let all = with_deprecated(
                    &t["inputFields"],
                    &self.extra["deprecatedInputFields"][name],
                );
                R::Many(Some(kept(all, args).into_iter().map(M::Input).collect()))
            }
            "ofType" => R::One(None),
            "specifiedByURL" => copy(facts, "specifiedByURL"),
            "isOneOf" => R::Leaf(facts.get("isOneOf").cloned().unwrap_or(Value::Bool(false))),
            _ => copy(t, field),
        }
    }
}
