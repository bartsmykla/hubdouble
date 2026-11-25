use apollo_compiler::Node;
use apollo_compiler::ast::{Definition, Document, Field, Selection, Type, Value as Ast};
use apollo_compiler::diagnostic::{Diagnostic, ToCliReport};
use apollo_compiler::executable::Operation;
use apollo_compiler::parser::{SourceMap, SourceSpan};
use apollo_compiler::schema::ExtendedType;
use apollo_compiler::validation::DiagnosticData;
use serde_json::{Map, Value, json};

use crate::{ErrorType, ResponseError, Schema};

pub(crate) fn validation(
    schema: &Schema,
    doc: &Document,
    diagnostic: &Diagnostic<'_, DiagnosticData>,
    fallback: ResponseError,
) -> ResponseError {
    let (Some(kind), Some(span)) = (
        diagnostic.error.unstable_error_name(),
        diagnostic.error.location(),
    ) else {
        return fallback;
    };
    let error = Validation {
        schema,
        sources: &doc.sources,
        span,
        kind,
    };
    doc.definitions
        .iter()
        .find_map(|def| match def {
            Definition::OperationDefinition(op) if covers(op, span) => {
                let keyword = op.operation_type.name();
                let root = schema.inner.root_operation(op.operation_type)?;
                let name = op
                    .name
                    .as_ref()
                    .map_or_else(|| keyword.to_owned(), |name| format!("{keyword} {name}"));
                error.search(root, &op.selection_set, &mut vec![json!(name)])
            }
            Definition::FragmentDefinition(f) if covers(f, span) => error.search(
                &f.type_condition,
                &f.selection_set,
                &mut vec![json!(format!("fragment {}", f.name))],
            ),
            _ => None,
        })
        .unwrap_or(fallback)
}

struct Validation<'a> {
    schema: &'a Schema,
    sources: &'a SourceMap,
    span: SourceSpan,
    kind: &'a str,
}

impl Validation<'_> {
    fn search(
        &self,
        parent: &str,
        selection: &[Selection],
        path: &mut Vec<Value>,
    ) -> Option<ResponseError> {
        for selected in selection {
            match selected {
                Selection::Field(field) if covers(field, self.span) => {
                    path.push(json!(field.alias.as_ref().unwrap_or(&field.name)));
                    let result = self.field(parent, field, path);
                    path.pop();
                    return result;
                }
                Selection::InlineFragment(fragment) if covers(fragment, self.span) => {
                    return self.search(
                        fragment
                            .type_condition
                            .as_ref()
                            .map_or(parent, |n| n.as_str()),
                        &fragment.selection_set,
                        path,
                    );
                }
                _ => {}
            }
        }
        None
    }

    fn field(
        &self,
        parent: &str,
        field: &Node<Field>,
        path: &mut Vec<Value>,
    ) -> Option<ResponseError> {
        let name = field.name.as_str();
        let at = field.location();
        let def = match self.schema.inner.type_field(parent, name) {
            Ok(def) => def,
            Err(_) if self.kind == "UndefinedField" => {
                return Some(self.error(
                    path,
                    at,
                    format!("Field '{name}' doesn't exist on type '{parent}'"),
                    json!({"code":"undefinedField","typeName":parent,"fieldName":name}),
                ));
            }
            Err(_) => return None,
        };
        if self.kind == "RequiredArgument" && at.is_some_and(|at| at.offset() == self.span.offset())
        {
            let required = def
                .arguments
                .iter()
                .filter(|arg| {
                    arg.ty.is_non_null()
                        && arg.default_value.is_none()
                        && !field.arguments.iter().any(|given| given.name == arg.name)
                })
                .map(|arg| arg.name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            if !required.is_empty() {
                return Some(self.error(path, at, format!("Field '{name}' is missing required arguments: {required}"),
                    json!({"code":"missingRequiredArguments","className":"Field","name":name,"arguments":required})));
            }
        }
        if let Some(arg) = field.arguments.iter().find(|a| covers(a, self.span)) {
            let key = arg.name.as_str();
            path.push(json!(key));
            let error = match def.arguments.iter().find(|def| def.name == arg.name) {
                None if self.kind == "UndefinedArgument" => Some(self.unknown_argument(path, arg.location(), "Field", name, key)),
                Some(def) if self.kind == "UnsupportedValueType" && !matches!(arg.value.as_ref(), Ast::Object(_) | Ast::List(_)) => {
                    Some(self.error(path, at,
                        format!("Argument '{key}' on Field '{name}' has an invalid value ({}). Expected type '{}'.", arg.value, def.ty),
                        json!({"code":"argumentLiteralsIncompatible","typeName":"Field","argumentName":key})))
                }
                Some(def) => self.input(&def.ty, &arg.value, path),
                None => None,
            };
            path.pop();
            return error;
        }
        self.search(def.ty.inner_named_type(), &field.selection_set, path)
    }

    fn input(&self, ty: &Type, value: &Node<Ast>, path: &mut Vec<Value>) -> Option<ResponseError> {
        if let (Type::List(item) | Type::NonNullList(item), Ast::List(values)) =
            (ty, value.as_ref())
        {
            let (i, child) = values
                .iter()
                .enumerate()
                .find(|(_, v)| covers(*v, self.span))?;
            path.push(json!(i));
            let result = self.input(item, child, path);
            path.pop();
            return result;
        }
        let name = ty.inner_named_type();
        let input = self.schema.inner.get_input_object(name)?;
        let Ast::Object(fields) = value.as_ref() else {
            return None;
        };
        if self.kind == "RequiredField"
            && value
                .location()
                .is_some_and(|at| at.offset() == self.span.offset())
        {
            let missing = input.fields.values().find(|def| {
                def.ty.is_non_null()
                    && def.default_value.is_none()
                    && !fields.iter().any(|(key, _)| *key == def.name)
            })?;
            path.push(json!(missing.name));
            let result = self.error(
                path,
                value.location(),
                format!(
                    "Argument '{}' on InputObject '{name}' is required. Expected type {}!",
                    missing.name,
                    missing.ty.to_string().trim_end_matches('!')
                ),
                json!({"code":"missingRequiredInputObjectAttribute","argumentName":missing.name,
                    "argumentType":missing.ty.to_string(),"inputObjectType":name}),
            );
            path.pop();
            return Some(result);
        }
        let (key, field) = fields.iter().find(|(k, v)| {
            covers(v, self.span)
                || k.location()
                    .is_some_and(|at| at.offset() == self.span.offset())
        })?;
        path.push(json!(key));
        let result = match input.fields.get(key) {
            Some(def) => self.input(&def.ty, field, path),
            None if self.kind == "UndefinedInputValue" => {
                Some(self.unknown_argument(path, key.location(), "InputObject", name, key))
            }
            None => None,
        };
        path.pop();
        result
    }

    fn unknown_argument(
        &self,
        path: &[Value],
        at: Option<SourceSpan>,
        class: &str,
        name: &str,
        argument: &str,
    ) -> ResponseError {
        self.error(path, at, format!("{class} '{name}' doesn't accept argument '{argument}'"),
            json!({"code":"argumentNotAccepted","name":name,"typeName":class,"argumentName":argument}))
    }

    fn error(
        &self,
        path: &[Value],
        at: Option<SourceSpan>,
        message: String,
        extensions: Value,
    ) -> ResponseError {
        ResponseError {
            kind: ErrorType::Validation,
            message,
            path: path.to_vec(),
            locations: locations(at, self.sources),
            extensions: Some(extensions),
        }
    }
}

fn covers<T: ?Sized>(node: &Node<T>, span: SourceSpan) -> bool {
    node.location().is_some_and(|at| {
        at.file_id() == span.file_id()
            && at.offset() <= span.offset()
            && span.offset() < at.end_offset()
    })
}

pub(crate) fn locations(at: Option<SourceSpan>, sources: &SourceMap) -> Vec<Value> {
    at.and_then(|l| l.line_column_range(sources))
        .map_or_else(Vec::new, |r| {
            vec![json!({"line": r.start.line, "column": r.start.column})]
        })
}

pub(crate) fn variables(
    schema: &Schema,
    operation: &Operation,
    given: &Map<String, Value>,
    sources: &SourceMap,
) -> Vec<ResponseError> {
    operation
        .variables
        .iter()
        .filter_map(|var| {
            let value = match given.get(var.name.as_str()) {
                Some(value) => value,
                None if var.default_value.is_some() => return None,
                None => &Value::Null,
            };
            let mut found = Vec::new();
            problems(schema, &var.ty, value, &mut Vec::new(), &mut found);
            (!found.is_empty()).then(|| ResponseError {
                kind: ErrorType::Validation,
                message: format!(
                    "Variable ${} of type {} was provided invalid value",
                    var.name, var.ty
                ),
                path: Vec::new(),
                locations: locations(var.location(), sources),
                extensions: Some(json!({"value":crate::scalar::shown(value),"problems":found})),
            })
        })
        .collect()
}

fn problems(
    schema: &Schema,
    ty: &Type,
    value: &Value,
    path: &mut Vec<Value>,
    out: &mut Vec<Value>,
) {
    if value.is_null() {
        if ty.is_non_null() {
            out.push(json!({"path":path,"explanation":"Expected value to not be null"}));
        }
        return;
    }
    if let Type::List(item) | Type::NonNullList(item) = ty {
        let values = value
            .as_array()
            .map_or_else(|| std::slice::from_ref(value), Vec::as_slice);
        for (i, child) in values.iter().enumerate() {
            path.push(json!(i));
            problems(schema, item, child, path, out);
            path.pop();
        }
        return;
    }
    let name = ty.inner_named_type();
    let fits = match schema.inner.types.get(name) {
        Some(ExtendedType::InputObject(def)) => {
            if let Some(fields) = value
                .as_object()
                .filter(|_| crate::scalar::wide_digits(value).is_none())
            {
                input_problems(schema, name, def, fields, path, out);
                return;
            }
            false
        }
        Some(ExtendedType::Enum(e)) => value.as_str().is_some_and(|s| e.values.contains_key(s)),
        Some(ExtendedType::Scalar(_)) => match name.as_str() {
            "ID" => value.is_string() || value.as_i64().is_some(),
            "Int" | "String" | "Boolean" | "Float" | "GitObjectID" | "GitRefname"
            | "Base64String" => crate::scalar::matches(name, value),
            _ => true,
        },
        _ => false,
    };
    if !fits {
        let mut shown = String::new();
        literal(value, &mut shown);
        out.push(
            json!({"path":path,"explanation":format!("Could not coerce value {shown} to {name}")}),
        );
    }
}

fn literal(value: &Value, out: &mut String) {
    if let Some(digits) = crate::scalar::wide_digits(value) {
        out.push_str(digits);
        return;
    }
    match value {
        Value::Object(fields) => {
            out.push('{');
            for (i, (key, value)) in fields.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(crate::scalar::unescaped(key));
                out.push(':');
                literal(value, out);
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                literal(item, out);
            }
            out.push(']');
        }
        v => out.push_str(&v.to_string()),
    }
}

fn input_problems(
    schema: &Schema,
    name: &str,
    def: &apollo_compiler::schema::InputObjectType,
    fields: &Map<String, Value>,
    path: &mut Vec<Value>,
    out: &mut Vec<Value>,
) {
    for key in fields
        .keys()
        .filter(|key| !def.fields.contains_key(key.as_str()))
    {
        path.push(json!(crate::scalar::unescaped(key)));
        out.push(json!({"path":path,"explanation":format!("Field is not defined on {name}")}));
        path.pop();
    }
    for field in def.fields.values() {
        let value = match fields.get(field.name.as_str()) {
            Some(value) => value,
            None if field.default_value.is_some() => continue,
            None => &Value::Null,
        };
        path.push(json!(field.name));
        problems(schema, &field.ty, value, path, out);
        path.pop();
    }
}
