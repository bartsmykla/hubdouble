use std::collections::{BTreeMap, HashSet};

use apollo_compiler::ExecutableDocument;
use apollo_compiler::ast::Type;
use apollo_compiler::executable::{Field, Selection, SelectionSet};
use apollo_compiler::schema::ExtendedType;
use serde_json::{Map, Value};

use crate::Schema;
use crate::selection::included;

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct AnswerError(String);

type Object = Map<String, Value>;
type Fields<'a> = BTreeMap<&'a str, Vec<&'a Field>>;

impl Schema {
    /// Checks an HTTP 200 GraphQL body against the requested operation and this schema.
    ///
    /// # Errors
    ///
    /// The envelope, a selected response position, or an error does not match the schema.
    pub fn check_answer(
        &self,
        query: &str,
        variables: &Value,
        body: &Value,
    ) -> Result<(), AnswerError> {
        let envelope = object(body, &[])?;
        keys(envelope, &["data", "errors", "extensions"])?;
        if let Some(ext) = envelope.get("extensions") {
            object(ext, &[])?;
        }
        let errors = read_errors(envelope.get("errors"), query)?;
        let check = || {
            let doc = crate::validate::validate(self, query).ok()?;
            let operation = doc.operations.get(None).ok()?;
            let given = variables.as_object().cloned().unwrap_or_default();
            let vars = crate::exec::variables(self, operation, &given, &doc.sources).ok()?;
            Some((doc, vars))
        };
        let Some((doc, vars)) = check() else {
            return demand(
                !envelope.contains_key("data")
                    && !errors.is_empty()
                    && errors.iter().all(|e| e.kind.is_none()),
                &[],
                "request errors without data",
            );
        };
        let operation = doc
            .operations
            .get(None)
            .map_err(|e| AnswerError(e.message().to_string()))?;
        let root = [&operation.selection_set];
        let check = Check {
            schema: self,
            doc: &doc,
            vars: &vars,
            errors: &errors,
        };
        let ty = Type::Named(operation.selection_set.ty.clone());
        let data = envelope.get("data");
        if let Err(expected) = crate::limits::check(&doc, operation, &vars) {
            let kind = (!expected.kind.is_validation()).then(|| expected.kind.as_str());
            return demand(
                data.is_none()
                    && errors.len() == 1
                    && errors[0].path.is_empty()
                    && errors[0].kind == kind,
                &[],
                "a resource limit error",
            );
        }
        let data = data.ok_or_else(|| invalid(&[], "data or request errors"))?;
        for error in &errors {
            demand(
                !error.path.is_empty() && check.trace(&root, &ty, Some(data), error.path, false),
                error.path,
                "a selected field or list item that resolved to null",
            )?;
        }
        if data.is_null() {
            demand(
                check.bubbled(&root, &ty, &[]),
                &[],
                "a non-null field error",
            )?;
            return Ok(());
        }
        check.selected(&root, &ty, data, &[])
    }
}

struct ErrorAt<'a> {
    kind: Option<&'a str>,
    path: &'a [Value],
}

fn read_errors<'a>(
    errors: Option<&'a Value>,
    query: &str,
) -> Result<Vec<ErrorAt<'a>>, AnswerError> {
    let Some(errors) = errors else {
        return Ok(Vec::new());
    };
    let array = errors
        .as_array()
        .filter(|a| !a.is_empty())
        .ok_or_else(|| invalid(&[], "a non-empty errors array"))?;
    array.iter().map(|error| read_error(error, query)).collect()
}

fn read_error<'a>(error: &'a Value, query: &str) -> Result<ErrorAt<'a>, AnswerError> {
    let error = object(error, &[])?;
    keys(
        error,
        &["message", "type", "path", "locations", "extensions"],
    )?;
    demand(nonempty(error.get("message")), &[], "an error message")?;
    let kind = error
        .get("type")
        .map(|v| {
            v.as_str()
                .filter(|s| {
                    matches!(
                        *s,
                        "NOT_FOUND"
                            | "FORBIDDEN"
                            | "UNPROCESSABLE"
                            | "INTERNAL"
                            | "HUBDOUBLE_NOT_IMPLEMENTED"
                            | "MISSING_PAGINATION_BOUNDARIES"
                            | "EXCESSIVE_PAGINATION"
                            | "INVALID_PAGINATION"
                            | "INVALID_CURSOR_ARGUMENTS"
                            | "MAX_NODE_LIMIT_EXCEEDED"
                            | "VALIDATION"
                    )
                })
                .ok_or_else(|| invalid(&[], "a GitHub error type"))
        })
        .transpose()?;
    let path = error
        .get("path")
        .map(|v| {
            v.as_array()
                .filter(|p| {
                    !p.is_empty()
                        && p[0].is_string()
                        && p.iter().all(|s| {
                            s.as_str().is_some_and(|s| !s.is_empty()) || s.as_u64().is_some()
                        })
                })
                .map(Vec::as_slice)
                .ok_or_else(|| invalid(&[], "an error path of aliases and indices"))
        })
        .transpose()?
        .unwrap_or_default();
    if let Some(ext) = error.get("extensions") {
        let ext = object(ext, path)?;
        if ext.contains_key("code") {
            demand(nonempty(ext.get("code")), path, "an error code")?;
        }
    }
    if let Some(locations) = error.get("locations") {
        let locations = locations
            .as_array()
            .filter(|a| !a.is_empty())
            .ok_or_else(|| invalid(path, "a non-empty locations array"))?;
        for location in locations {
            check_location(location, query, path)?;
        }
    } else {
        demand(path.is_empty(), path, "error locations")?;
    }
    Ok(ErrorAt { kind, path })
}

fn check_location(value: &Value, query: &str, path: &[Value]) -> Result<(), AnswerError> {
    let location = object(value, path)?;
    keys(location, &["line", "column"])?;
    let positive = |key| {
        location
            .get(key)
            .and_then(Value::as_u64)
            .and_then(|n| n.checked_sub(1))
            .and_then(|n| usize::try_from(n).ok())
    };
    let line = positive("line").and_then(|i| query.split('\n').nth(i));
    demand(
        line.zip(positive("column"))
            .is_some_and(|(line, col)| col <= line.chars().count()),
        path,
        "an error location in the document",
    )
}

fn nonempty(value: Option<&Value>) -> bool {
    value
        .and_then(Value::as_str)
        .is_some_and(|s| !s.trim().is_empty())
}

fn keys(object: &Object, allowed: &[&str]) -> Result<(), AnswerError> {
    demand(
        object.keys().all(|k| allowed.contains(&k.as_str())),
        &[],
        "known envelope keys",
    )
}

fn object<'a>(value: &'a Value, path: &[Value]) -> Result<&'a Object, AnswerError> {
    value
        .as_object()
        .ok_or_else(|| invalid(path, "a JSON object"))
}

fn invalid(path: &[Value], expected: &str) -> AnswerError {
    AnswerError(format!("GraphQL answer at {path:?} requires {expected}"))
}

fn demand(ok: bool, path: &[Value], expected: &str) -> Result<(), AnswerError> {
    if ok {
        Ok(())
    } else {
        Err(invalid(path, expected))
    }
}

struct Check<'a> {
    schema: &'a Schema,
    doc: &'a ExecutableDocument,
    vars: &'a Object,
    errors: &'a [ErrorAt<'a>],
}

impl<'a> Check<'a> {
    fn selected(
        &self,
        sets: &[&'a SelectionSet],
        ty: &Type,
        value: &Value,
        path: &[Value],
    ) -> Result<(), AnswerError> {
        if value.is_null() {
            demand(!ty.is_non_null(), path, &ty.to_string())?;
            if self
                .errors
                .iter()
                .any(|e| e.path.starts_with(path) && e.path.len() > path.len())
            {
                demand(
                    self.bubbled(sets, ty, path),
                    path,
                    "a non-null descendant error",
                )?;
            }
            return Ok(());
        }
        if let Type::List(item) | Type::NonNullList(item) = ty {
            let list = value
                .as_array()
                .ok_or_else(|| invalid(path, &ty.to_string()))?;
            for (i, v) in list.iter().enumerate() {
                self.selected(sets, item, v, &push(path, Value::from(i)))?;
            }
            return Ok(());
        }
        let name = ty.inner_named_type().as_str();
        match self.schema.inner.types.get(name) {
            Some(ExtendedType::Scalar(_)) => {
                demand(crate::scalar::matches(name, value), path, name)
            }
            Some(ExtendedType::Enum(e)) => demand(
                value.as_str().is_some_and(|s| e.values.contains_key(s)),
                path,
                name,
            ),
            _ => self.composite(sets, name, object(value, path)?, path),
        }
    }

    fn composite(
        &self,
        sets: &[&'a SelectionSet],
        name: &str,
        body: &Object,
        path: &[Value],
    ) -> Result<(), AnswerError> {
        let mut last = invalid(path, name);
        for concrete in self.candidates(name) {
            match self.fields_match(body, concrete, &self.fields(sets, concrete), path) {
                Ok(()) => return Ok(()),
                Err(e) => last = e,
            }
        }
        Err(last)
    }

    fn fields_match(
        &self,
        body: &Object,
        concrete: &str,
        fields: &Fields<'a>,
        path: &[Value],
    ) -> Result<(), AnswerError> {
        demand(
            body.keys().all(|k| fields.contains_key(k.as_str())),
            path,
            "only selected fields",
        )?;
        for (&key, selected) in fields {
            let path = push(path, Value::from(key));
            let value = body
                .get(key)
                .ok_or_else(|| invalid(&path, "the selected field"))?;
            let field = selected[0];
            if field.name == "__typename" {
                demand(value.as_str() == Some(concrete), &path, concrete)?;
            } else {
                self.selected(&subsets(selected), &field.definition.ty, value, &path)?;
            }
        }
        Ok(())
    }

    fn bubbled(&self, sets: &[&'a SelectionSet], ty: &Type, path: &[Value]) -> bool {
        self.errors.iter().any(|e| {
            e.path.strip_prefix(path).is_some_and(|suffix| {
                !suffix.is_empty() && self.trace(sets, ty, None, suffix, true)
            })
        })
    }

    fn trace(
        &self,
        sets: &[&'a SelectionSet],
        ty: &Type,
        data: Option<&Value>,
        path: &[Value],
        non_null: bool,
    ) -> bool {
        let Some((seg, rest)) = path.split_first() else {
            return data.is_none_or(Value::is_null);
        };
        let data = data.filter(|v| !v.is_null());
        if let Type::List(item) | Type::NonNullList(item) = ty {
            let Some(i) = seg.as_u64().and_then(|n| usize::try_from(n).ok()) else {
                return false;
            };
            let child = data.and_then(|v| v.get(i));
            return (!non_null || item.is_non_null())
                && (data.is_none() || child.is_some())
                && self.trace(sets, item, child, rest, non_null);
        }
        let Some(key) = seg.as_str() else {
            return false;
        };
        self.candidates(ty.inner_named_type())
            .into_iter()
            .any(|concrete| {
                let fields = self.fields(sets, concrete);
                if let Some(data) = data
                    && fields.iter().any(|(k, f)| {
                        f[0].name == "__typename" && data[*k].as_str() != Some(concrete)
                    })
                {
                    return false;
                }
                let Some(f) = fields.get(key) else {
                    return false;
                };
                let ty = &f[0].definition.ty;
                let child = data.and_then(|v| v.get(key));
                (!non_null || ty.is_non_null())
                    && (data.is_none() || child.is_some())
                    && self.trace(&subsets(f), ty, child, rest, non_null)
            })
    }

    fn candidates<'s>(&'s self, name: &'s str) -> Vec<&'s str> {
        match self.schema.inner.types.get(name) {
            Some(ExtendedType::Object(_)) => vec![name],
            Some(ExtendedType::Union(u)) => u.members.iter().map(|n| n.as_str()).collect(),
            Some(ExtendedType::Interface(_)) => self
                .schema
                .inner
                .types
                .iter()
                .filter_map(|(n, ty)| {
                    (matches!(ty, ExtendedType::Object(_)) && self.applies(n, name))
                        .then_some(n.as_str())
                })
                .collect(),
            _ => Vec::new(),
        }
    }

    fn applies(&self, concrete: &str, condition: &str) -> bool {
        concrete == condition || self.schema.inner.is_subtype(condition, concrete)
    }

    fn fields(&self, sets: &[&'a SelectionSet], concrete: &str) -> Fields<'a> {
        let mut visited = HashSet::new();
        let mut fields = BTreeMap::new();
        for set in sets {
            self.collect(set, concrete, &mut fields, &mut visited);
        }
        fields
    }

    fn collect(
        &self,
        set: &'a SelectionSet,
        concrete: &str,
        fields: &mut Fields<'a>,
        visited: &mut HashSet<&'a str>,
    ) {
        for selection in &set.selections {
            match selection {
                Selection::Field(f) if included(&f.directives, self.vars) => {
                    fields.entry(f.response_key().as_str()).or_default().push(f);
                }
                Selection::InlineFragment(f)
                    if included(&f.directives, self.vars)
                        && f.type_condition
                            .as_ref()
                            .is_none_or(|c| self.applies(concrete, c)) =>
                {
                    self.collect(&f.selection_set, concrete, fields, visited);
                }
                Selection::FragmentSpread(f)
                    if included(&f.directives, self.vars)
                        && visited.insert(f.fragment_name.as_str()) =>
                {
                    if let Some(fragment) = self.doc.fragments.get(&f.fragment_name)
                        && self.applies(concrete, &fragment.selection_set.ty)
                    {
                        self.collect(&fragment.selection_set, concrete, fields, visited);
                    }
                }
                _ => {}
            }
        }
    }
}

fn subsets<'a>(fields: &[&'a Field]) -> Vec<&'a SelectionSet> {
    fields.iter().map(|f| &f.selection_set).collect()
}

fn push(path: &[Value], segment: Value) -> Vec<Value> {
    let mut next = path.to_vec();
    next.push(segment);
    next
}
