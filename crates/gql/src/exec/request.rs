use apollo_compiler::ast::OperationType;
use apollo_compiler::executable::Operation;
use apollo_compiler::parser::SourceMap;
use apollo_compiler::request::RequestError;
use apollo_compiler::response::JsonMap;
use hubdouble_auth::Actor;
use hubdouble_git::Git;
use hubdouble_model::ApiError;
use hubdouble_store::Store;
use serde_json::{Map, Value};

use super::{Exec, QUERY};
use crate::contract::{Ctx, ErrorType, Obj, Registry, Request, Response, ResponseError};
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
    };
    let data = match op.operation_type {
        OperationType::Query => store.view(|tx| {
            let cx = Ctx {
                tx,
                git,
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
        OperationType::Mutation => ex.mutations(store, git, actor, request, &op.selection_set),
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
    let extensions = (!ex.warnings.is_empty()).then(|| {
        let mut m = Map::new();
        m.insert("warnings".to_owned(), Value::Array(ex.warnings));
        m
    });
    Ok(Response {
        data: Some(data.map_or(Value::Null, Value::Object)),
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
    let raw: JsonMap = serde_json::from_value(Value::Object(given.clone())).unwrap_or_default();
    let coerced = apollo_compiler::request::coerce_variable_values(&schema.inner, op, &raw)
        .map_err(|e| vec![request_error(&e, sources)])?;
    Ok(match serde_json::to_value(&*coerced) {
        Ok(Value::Object(m)) => m,
        _ => Map::new(),
    })
}
