use apollo_compiler::ast::OperationType;
use apollo_compiler::executable::Operation;
use apollo_compiler::parser::SourceMap;
use apollo_compiler::request::RequestError;
use apollo_compiler::response::JsonMap;
use hubdouble_auth::Actor;
use hubdouble_git::{Git, ReadJobs};
use hubdouble_model::ApiError;
use hubdouble_store::Store;
use serde_json::{Map, Value};

use super::{Exec, QUERY};
use crate::contract::{Ctx, ErrorType, Obj, Registry, Request, Response, ResponseError};
use crate::data::Data;
use crate::request_errors::locations;

pub(crate) fn execute(
    registry: &Registry,
    store: &Store,
    git: &Git,
    actor: &Actor,
    request: &Request<'_>,
) -> Result<Response, ApiError> {
    let doc = match crate::validate::validate(&registry.schema, request.query) {
        Ok(doc) => doc,
        Err(errors) => return Ok(failed(errors)),
    };
    let op = match doc.operations.get(request.operation_name) {
        Ok(op) => op,
        Err(e) => return Ok(failed(vec![request_error(&e, &doc.sources)])),
    };
    let vars = match variables(&registry.schema, op, request.variables, &doc.sources) {
        Ok(vars) => vars,
        Err(errors) => return Ok(failed(errors)),
    };
    if let Err(error) = crate::limits::check(&doc, op, &vars) {
        return Ok(failed(vec![error]));
    }
    let mut ex = Exec {
        registry,
        doc: &doc,
        vars: &vars,
        errors: Vec::new(),
        warnings: Vec::new(),
        omit_data: false,
    };
    let read_jobs = ReadJobs::default();
    let data = match op.operation_type {
        OperationType::Query => store.view(|tx| {
            let cx = Ctx {
                tx,
                git,
                read_jobs: &read_jobs,
                actor,
                clock: request.clock,
                urls: request.urls,
                registry,
            };
            let root = Obj {
                type_name: QUERY,
                value: registry.root.clone(),
            };
            Ok(ex.root(&cx, &root, &op.selection_set))
        })?,
        OperationType::Mutation => {
            ex.mutations(store, (git, &read_jobs), actor, request, &op.selection_set)
        }
        OperationType::Subscription => {
            return Ok(failed(vec![ResponseError {
                kind: ErrorType::Validation,
                message: "Subscriptions are not supported.".to_owned(),
                path: Vec::new(),
                locations: Vec::new(),
                extensions: None,
            }]));
        }
    };
    read_jobs.run(git, store)?;
    let extensions = (!ex.warnings.is_empty()).then(|| {
        let mut m = Map::new();
        m.insert("warnings".to_owned(), Value::Array(ex.warnings));
        m
    });
    Ok(Response {
        data: (!ex.omit_data).then(|| data.map_or(Data::NULL, Data::Object)),
        errors: ex.errors,
        extensions,
    })
}

fn failed(errors: Vec<ResponseError>) -> Response {
    Response {
        data: None,
        errors,
        extensions: None,
    }
}

fn request_error(e: &RequestError, sources: &SourceMap) -> ResponseError {
    ResponseError {
        kind: ErrorType::Validation,
        message: e.message().to_string(),
        path: Vec::new(),
        locations: locations(e.location(), sources),
        extensions: None,
    }
}

pub(crate) fn variables(
    schema: &crate::Schema,
    op: &Operation,
    given: &Map<String, Value>,
    sources: &SourceMap,
) -> Result<Map<String, Value>, Vec<ResponseError>> {
    let errors = crate::request_errors::variables(schema, op, given, sources);
    if !errors.is_empty() {
        return Err(errors);
    }
    let mut masked = given.clone();
    masked.values_mut().for_each(mask_wide_ints);
    let raw: JsonMap = serde_json::from_value(Value::Object(masked)).unwrap_or_default();
    let coerced = apollo_compiler::request::coerce_variable_values(&schema.inner, op, &raw)
        .map_err(|e| vec![request_error(&e, sources)])?;
    let Ok(Value::Object(mut coerced)) = serde_json::to_value(&*coerced) else {
        return Ok(Map::new());
    };
    for (name, value) in given {
        if let Some(c) = coerced.get_mut(name) {
            restore_wide_ints(c, value);
        }
    }
    Ok(coerced)
}

fn wide_int(v: &Value) -> bool {
    match v {
        Value::Number(n) => !n.is_f64() && n.as_i64().is_none_or(|n| i32::try_from(n).is_err()),
        v => crate::scalar::wide_digits(v).is_some(),
    }
}

fn mask_wide_ints(v: &mut Value) {
    match v {
        v if wide_int(v) => *v = Value::from(0),
        Value::Array(items) => items.iter_mut().for_each(mask_wide_ints),
        Value::Object(fields) => fields.values_mut().for_each(mask_wide_ints),
        _ => {}
    }
}

fn restore_wide_ints(coerced: &mut Value, given: &Value) {
    match (coerced, given) {
        (c @ Value::Number(_), g) if wide_int(g) => *c = g.clone(),
        (Value::Array(c), Value::Array(g)) => {
            for (c, g) in c.iter_mut().zip(g) {
                restore_wide_ints(c, g);
            }
        }
        (Value::Array(c), g) if c.len() == 1 => restore_wide_ints(&mut c[0], g),
        (Value::Object(c), Value::Object(g)) => {
            for (k, g) in g {
                if let Some(c) = c.get_mut(k) {
                    restore_wide_ints(c, g);
                }
            }
        }
        _ => {}
    }
}
