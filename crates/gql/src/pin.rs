use std::fmt::Write as _;

use serde_json::Value;

use crate::contract::{ErrorType, GqlError};

pub(crate) const BUILT_IN_SCALARS: [&str; 5] = ["Boolean", "Float", "ID", "Int", "String"];
const BUILT_IN_DIRECTIVES: [&str; 4] = ["deprecated", "include", "skip", "specifiedBy"];
const DEFAULT_REASON: &str = "No longer supported";

pub(crate) fn invalid(what: &str) -> GqlError {
    GqlError::new(ErrorType::Internal, format!("the GraphQL pin has {what}"))
}

pub(crate) fn is_built_in(name: &str) -> bool {
    name.starts_with("__") || BUILT_IN_SCALARS.contains(&name)
}

/// The values a list of the pin holds once the deprecated entries that the live `GET /graphql`
/// leaves out are put back at their live positions.
pub(crate) fn with_deprecated<'a>(listed: &'a Value, deprecated: &'a Value) -> Vec<&'a Value> {
    let mut out: Vec<&Value> = listed.as_array().into_iter().flatten().collect();
    for entry in deprecated.as_array().into_iter().flatten() {
        let index = entry["index"]
            .as_u64()
            .and_then(|i| usize::try_from(i).ok())
            .unwrap_or(out.len())
            .min(out.len());
        out.insert(index, &entry["value"]);
    }
    out
}

fn string(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if u32::from(c) < 0x20 => {
                let _ = write!(out, "\\u{:04X}", u32::from(c));
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

fn description(out: &mut String, indent: &str, v: &Value) {
    if let Some(d) = v["description"].as_str() {
        out.push_str(indent);
        string(out, d);
        out.push('\n');
    }
}

fn type_ref(r: &Value) -> Result<String, GqlError> {
    match r["kind"].as_str() {
        Some("NON_NULL") => Ok(format!("{}!", type_ref(&r["ofType"])?)),
        Some("LIST") => Ok(format!("[{}]", type_ref(&r["ofType"])?)),
        _ => r["name"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| invalid("a type reference without a name")),
    }
}

fn deprecation(out: &mut String, v: &Value) {
    if v["isDeprecated"] == true {
        match v["deprecationReason"].as_str() {
            Some(DEFAULT_REASON) | None => out.push_str(" @deprecated"),
            Some(reason) => {
                out.push_str(" @deprecated(reason: ");
                string(out, reason);
                out.push(')');
            }
        }
    }
}

fn input_value(out: &mut String, indent: &str, v: &Value) -> Result<(), GqlError> {
    description(out, indent, v);
    let name = v["name"]
        .as_str()
        .ok_or_else(|| invalid("an input value without a name"))?;
    let _ = write!(out, "{indent}{name}: {}", type_ref(&v["type"])?);
    if let Some(default) = v["defaultValue"].as_str() {
        let _ = write!(out, " = {default}");
    }
    deprecation(out, v);
    out.push('\n');
    Ok(())
}

fn names(list: &Value) -> Vec<&str> {
    list.as_array()
        .into_iter()
        .flatten()
        .filter_map(|t| t["name"].as_str())
        .collect()
}

fn fields(out: &mut String, owner: &str, t: &Value, extra: &Value) -> Result<(), GqlError> {
    out.push_str(" {\n");
    for f in t["fields"].as_array().into_iter().flatten() {
        description(out, "  ", f);
        let name = f["name"]
            .as_str()
            .ok_or_else(|| invalid("a field without a name"))?;
        out.push_str("  ");
        out.push_str(name);
        let args = with_deprecated(
            &f["args"],
            &extra["deprecatedArgs"][format!("{owner}.{name}")],
        );
        if !args.is_empty() {
            out.push_str("(\n");
            for a in args {
                input_value(out, "    ", a)?;
            }
            out.push_str("  )");
        }
        let _ = write!(out, ": {}", type_ref(&f["type"])?);
        deprecation(out, f);
        out.push('\n');
    }
    out.push_str("}\n");
    Ok(())
}

fn implements(out: &mut String, t: &Value) {
    let interfaces = names(&t["interfaces"]);
    if !interfaces.is_empty() {
        let _ = write!(out, " implements {}", interfaces.join(" & "));
    }
}

fn definition(out: &mut String, t: &Value, extra: &Value) -> Result<(), GqlError> {
    let name = t["name"]
        .as_str()
        .ok_or_else(|| invalid("a type without a name"))?;
    description(out, "", t);
    match t["kind"].as_str() {
        Some("SCALAR") => {
            let _ = write!(out, "scalar {name}");
            if let Some(url) = extra["types"][name]["specifiedByURL"].as_str() {
                out.push_str(" @specifiedBy(url: ");
                string(out, url);
                out.push(')');
            }
            out.push('\n');
        }
        Some("OBJECT") => {
            let _ = write!(out, "type {name}");
            implements(out, t);
            fields(out, name, t, extra)?;
        }
        Some("INTERFACE") => {
            let _ = write!(out, "interface {name}");
            implements(out, t);
            fields(out, name, t, extra)?;
        }
        Some("UNION") => {
            let _ = writeln!(
                out,
                "union {name} = {}",
                names(&t["possibleTypes"]).join(" | ")
            );
        }
        Some("ENUM") => {
            let _ = writeln!(out, "enum {name} {{");
            for v in t["enumValues"].as_array().into_iter().flatten() {
                description(out, "  ", v);
                let value = v["name"]
                    .as_str()
                    .ok_or_else(|| invalid("an enum value without a name"))?;
                let _ = write!(out, "  {value}");
                deprecation(out, v);
                out.push('\n');
            }
            out.push_str("}\n");
        }
        Some("INPUT_OBJECT") => {
            let _ = write!(out, "input {name}");
            if extra["types"][name]["isOneOf"] == true {
                out.push_str(" @oneOf");
            }
            out.push_str(" {\n");
            for v in with_deprecated(&t["inputFields"], &extra["deprecatedInputFields"][name]) {
                input_value(out, "  ", v)?;
            }
            out.push_str("}\n");
        }
        _ => return Err(invalid("a type of an unknown kind")),
    }
    out.push('\n');
    Ok(())
}

fn extension(out: &mut String, live: &Value, extended: &str, known: &[&str]) {
    let added: Vec<&Value> = live["fields"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|f| f["name"].as_str().is_some_and(|n| !known.contains(&n)))
        .collect();
    if added.is_empty() {
        return;
    }
    let _ = writeln!(out, "extend type {extended} {{");
    for f in added {
        description(out, "  ", f);
        let name = f["name"].as_str().unwrap_or_default();
        let ty = type_ref(&f["type"]).unwrap_or_default();
        let _ = write!(out, "  {name}: {ty}");
        deprecation(out, f);
        out.push('\n');
    }
    out.push_str("}\n\n");
}

const APOLLO_TYPE_FIELDS: [&str; 10] = [
    "kind",
    "name",
    "description",
    "fields",
    "interfaces",
    "possibleTypes",
    "enumValues",
    "inputFields",
    "ofType",
    "specifiedByURL",
];
const APOLLO_DIRECTIVE_FIELDS: [&str; 5] =
    ["name", "description", "locations", "args", "isRepeatable"];

/// The SDL of the schema that live `GET /graphql` describes: `introspection` is that answer and
/// `extra` the facts that only an introspection `POST` shows (`xtask graphql-pin` writes both).
/// Types, fields, arguments, input fields and enum values keep the live order, descriptions keep
/// the live text, and the introspection fields GitHub adds to `__Type` and `__Directive` are
/// type extensions.
///
/// # Errors
///
/// An `INTERNAL` error when either pin is not the JSON that `xtask graphql-pin` writes.
pub fn schema_sdl(introspection: &str, extra: &str) -> Result<String, GqlError> {
    let answer: Value =
        serde_json::from_str(introspection).map_err(|_| invalid("no JSON answer"))?;
    let extra: Value = serde_json::from_str(extra).map_err(|_| invalid("no JSON extra facts"))?;
    let schema = &answer["data"]["__schema"];
    let types = schema["types"]
        .as_array()
        .ok_or_else(|| invalid("no types"))?;
    let mut out = String::with_capacity(introspection.len() / 2);
    let query = schema["queryType"]["name"]
        .as_str()
        .ok_or_else(|| invalid("no query type"))?;
    let _ = write!(out, "schema {{\n  query: {query}\n");
    if let Some(mutation) = schema["mutationType"]["name"].as_str() {
        let _ = writeln!(out, "  mutation: {mutation}");
    }
    if let Some(subscription) = schema["subscriptionType"]["name"].as_str() {
        let _ = writeln!(out, "  subscription: {subscription}");
    }
    out.push_str("}\n\n");
    for d in schema["directives"].as_array().into_iter().flatten() {
        let name = d["name"]
            .as_str()
            .ok_or_else(|| invalid("a directive without a name"))?;
        if BUILT_IN_DIRECTIVES.contains(&name) {
            continue;
        }
        description(&mut out, "", d);
        let _ = write!(out, "directive @{name}");
        let args = with_deprecated(&d["args"], &extra["deprecatedArgs"][format!("@{name}")]);
        if !args.is_empty() {
            out.push_str("(\n");
            for a in args {
                input_value(&mut out, "  ", a)?;
            }
            out.push(')');
        }
        if extra["directives"][name]["isRepeatable"] == true {
            out.push_str(" repeatable");
        }
        let _ = writeln!(
            out,
            " on {}\n",
            names_or_values(&d["locations"]).join(" | ")
        );
    }
    for t in types {
        let name = t["name"].as_str().unwrap_or_default();
        match name {
            "__Type" => extension(&mut out, t, name, &APOLLO_TYPE_FIELDS),
            "__Directive" => extension(&mut out, t, name, &APOLLO_DIRECTIVE_FIELDS),
            n if is_built_in(n) => {}
            _ => definition(&mut out, t, &extra)?,
        }
    }
    Ok(out)
}

fn names_or_values(list: &Value) -> Vec<&str> {
    list.as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect()
}
