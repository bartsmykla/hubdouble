use std::any::{Any, TypeId};
use std::sync::Arc;

use apollo_compiler::ast::{DirectiveList, OperationType, Type, Value as Ast};
use apollo_compiler::collections::IndexMap;
use apollo_compiler::executable::{Field, Operation, Selection, SelectionSet};
use apollo_compiler::parser::SourceSpan;
use apollo_compiler::request::RequestError;
use apollo_compiler::response::JsonMap;
use apollo_compiler::schema::{ExtendedType, FieldDefinition};
use apollo_compiler::{ExecutableDocument, Node};
use hubdouble_auth::Actor;
use hubdouble_git::Git;
use hubdouble_model::ApiError;
use hubdouble_store::Store;
use serde_json::{Map, Value, json};

use crate::contract::{
    Args, Conn, Ctx, Edge, ErrorType, GqlError, MutCtx, Obj, PageInfo, Registry, Request, Resolved,
    Response, ResponseError,
};

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
        Err(e) => return Ok(failed(vec![request_error(&e, &doc)])),
    };
    let vars = match variables(registry, op, request.variables) {
        Ok(vars) => vars,
        Err(e) => return Ok(failed(vec![request_error(&e, &doc)])),
    };
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

const QUERY: &str = "Query";
const MUTATION: &str = "Mutation";
const DEPRECATION: &str =
    "The id {} is deprecated. Update your cache to use the next_global_id from the data payload.";

fn failed(errors: Vec<ResponseError>) -> Response {
    Response {
        data: None,
        errors,
        extensions: None,
    }
}

fn request_error(e: &RequestError, doc: &ExecutableDocument) -> ResponseError {
    ResponseError {
        kind: ErrorType::Validation,
        message: e.message().to_string(),
        path: Vec::new(),
        locations: locations(e.location(), doc),
    }
}

fn locations(at: Option<SourceSpan>, doc: &ExecutableDocument) -> Vec<Value> {
    at.and_then(|l| l.line_column_range(&doc.sources))
        .map_or_else(Vec::new, |r| {
            vec![json!({"line": r.start.line, "column": r.start.column})]
        })
}

fn variables(
    registry: &Registry,
    op: &Operation,
    given: &Map<String, Value>,
) -> Result<Map<String, Value>, RequestError> {
    let raw: JsonMap = serde_json::from_value(Value::Object(given.clone())).unwrap_or_default();
    let coerced =
        apollo_compiler::request::coerce_variable_values(&registry.schema.inner, op, &raw)?;
    Ok(match serde_json::to_value(&*coerced) {
        Ok(Value::Object(m)) => m,
        _ => Map::new(),
    })
}

/// A position whose value became `null` because of an error already recorded.
struct Null;

#[derive(Clone, Copy)]
enum Seg<'p> {
    Key(&'p str),
    Index(usize),
}

#[derive(Clone, Copy)]
struct Path<'p> {
    parent: Option<&'p Path<'p>>,
    seg: Seg<'p>,
}

impl Path<'_> {
    fn segments(self) -> Vec<Value> {
        let mut out = Vec::new();
        let mut at = Some(&self);
        while let Some(p) = at {
            out.push(match p.seg {
                Seg::Key(k) => Value::String(k.to_owned()),
                Seg::Index(i) => Value::from(i),
            });
            at = p.parent;
        }
        out.reverse();
        out
    }
}

#[derive(Clone, Copy)]
enum Parent<'v> {
    Obj(&'v Obj),
    Edge(&'v Edge),
    PageInfo(&'v PageInfo),
}

enum Out<'v> {
    R(Resolved),
    Edges(&'v [Edge]),
    Nodes(&'v [Edge]),
    Edge(&'v Edge),
    PageInfo(&'v PageInfo),
}

type Fields<'d> = [&'d Node<Field>];
type Groups<'d> = IndexMap<&'d str, Vec<&'d Node<Field>>>;

struct Exec<'r> {
    registry: &'r Registry,
    doc: &'r ExecutableDocument,
    vars: &'r Map<String, Value>,
    errors: Vec<ResponseError>,
    warnings: Vec<Value>,
}

impl<'r> Exec<'r> {
    fn schema(&self) -> &'r apollo_compiler::Schema {
        &self.registry.schema.inner
    }

    fn root(
        &mut self,
        cx: &Ctx<'_>,
        root: &Obj,
        set: &'r SelectionSet,
    ) -> Result<Map<String, Value>, Null> {
        let mut groups = Groups::default();
        self.collect(root.type_name, set, &mut groups, &mut Vec::new());
        self.set(cx, Parent::Obj(root), root.type_name, &groups, None)
    }

    fn mutations(
        &mut self,
        store: &Store,
        git: &Git,
        actor: &Actor,
        request: &Request<'_>,
        set: &'r SelectionSet,
    ) -> Result<Map<String, Value>, Null> {
        let mut groups = Groups::default();
        self.collect(MUTATION, set, &mut groups, &mut Vec::new());
        let mut map = Map::new();
        let mut nulled = false;
        for (key, fields) in &groups {
            let path = Path {
                parent: None,
                seg: Seg::Key(key),
            };
            match self.mutation(store, git, actor, request, fields, &path) {
                Ok(v) => {
                    map.insert((*key).to_owned(), v);
                }
                Err(Null) => nulled = true,
            }
        }
        if nulled { Err(Null) } else { Ok(map) }
    }

    fn mutation(
        &mut self,
        store: &Store,
        git: &Git,
        actor: &Actor,
        request: &Request<'_>,
        fields: &Fields<'r>,
        path: &Path<'_>,
    ) -> Result<Value, Null> {
        let f = fields[0];
        let name = f.name.as_str();
        if name == "__typename" {
            return Ok(Value::String(MUTATION.to_owned()));
        }
        let Ok(def) = self.schema().type_field(MUTATION, name) else {
            self.error(
                ErrorType::Internal,
                format!("{MUTATION}.{name} is not in the schema"),
                f,
                path,
            );
            return Err(Null);
        };
        let args = self.args(f, def);
        let registry = self.registry;
        let Some(resolver) = registry.mutations.get(name).copied() else {
            self.error(
                ErrorType::NotImplemented,
                not_implemented(MUTATION, name),
                f,
                path,
            );
            return nulled(&def.ty);
        };
        let mut failure = None;
        let result = store.update(|tx| {
            let mut m = MutCtx {
                tx: tx.reborrow(),
                git,
                actor,
                clock: request.clock,
                urls: request.urls,
                registry,
            };
            match resolver(&mut m, &args) {
                Ok(r) => Ok(self.complete(&m.ctx(), MUTATION, &def.ty, Out::R(r), fields, path)),
                Err(e) => {
                    failure = Some(e);
                    Err(ApiError::internal("rolled back"))
                }
            }
        });
        match result {
            Ok(v) => v,
            Err(e) => {
                let e = failure.unwrap_or_else(|| GqlError::from(e));
                self.field_error(e, f, path);
                nulled(&def.ty)
            }
        }
    }

    fn collect(
        &self,
        type_name: &str,
        set: &'r SelectionSet,
        groups: &mut Groups<'r>,
        visited: &mut Vec<&'r str>,
    ) {
        for selection in &set.selections {
            match selection {
                Selection::Field(f) => {
                    if self.included(&f.directives) {
                        groups.entry(f.response_key().as_str()).or_default().push(f);
                    }
                }
                Selection::InlineFragment(inline) => {
                    if self.included(&inline.directives)
                        && inline
                            .type_condition
                            .as_ref()
                            .is_none_or(|c| self.applies(type_name, c))
                    {
                        self.collect(type_name, &inline.selection_set, groups, visited);
                    }
                }
                Selection::FragmentSpread(spread) => {
                    let name = spread.fragment_name.as_str();
                    if self.included(&spread.directives) && !visited.contains(&name) {
                        visited.push(name);
                        if let Some(fragment) = self.doc.fragments.get(name)
                            && self.applies(type_name, &fragment.selection_set.ty)
                        {
                            self.collect(type_name, &fragment.selection_set, groups, visited);
                        }
                    }
                }
            }
        }
    }

    fn included(&self, directives: &DirectiveList) -> bool {
        let flag = |name: &str| {
            directives
                .get(name)
                .and_then(|d| d.specified_argument_by_name("if"))
                .and_then(|v| self.json(v))
                .and_then(|v| v.as_bool())
        };
        flag("skip") != Some(true) && flag("include") != Some(false)
    }

    fn applies(&self, object: &str, condition: &str) -> bool {
        object == condition
            || match self.schema().types.get(condition) {
                Some(ExtendedType::Interface(_)) => self
                    .schema()
                    .get_object(object)
                    .is_some_and(|o| o.implements_interfaces.contains(condition)),
                Some(ExtendedType::Union(u)) => u.members.contains(object),
                _ => false,
            }
    }

    fn set(
        &mut self,
        cx: &Ctx<'_>,
        parent: Parent<'_>,
        type_name: &str,
        groups: &Groups<'r>,
        path: Option<&Path<'_>>,
    ) -> Result<Map<String, Value>, Null> {
        let mut map = Map::new();
        let mut serde = None;
        let mut nulled = false;
        for (key, fields) in groups {
            let path = Path {
                parent: path,
                seg: Seg::Key(key),
            };
            match self.field(cx, parent, type_name, fields, &path, &mut serde) {
                Ok(v) => {
                    map.insert((*key).to_owned(), v);
                }
                Err(Null) => nulled = true,
            }
        }
        if nulled { Err(Null) } else { Ok(map) }
    }

    fn field(
        &mut self,
        cx: &Ctx<'_>,
        parent: Parent<'_>,
        type_name: &str,
        fields: &Fields<'r>,
        path: &Path<'_>,
        serde: &mut Option<Value>,
    ) -> Result<Value, Null> {
        let f = fields[0];
        let name = f.name.as_str();
        if name == "__typename" {
            return Ok(Value::String(type_name.to_owned()));
        }
        let Ok(def) = self.schema().type_field(type_name, name) else {
            self.error(
                ErrorType::NotImplemented,
                not_implemented(type_name, name),
                f,
                path,
            );
            return Err(Null);
        };
        let args = self.args(f, def);
        match self.resolve(cx, parent, type_name, def, &args, f, path, serde) {
            Ok(out) => self.complete(cx, type_name, &def.ty, out, fields, path),
            Err(e) => {
                self.field_error(e, f, path);
                nulled(&def.ty)
            }
        }
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "one resolution step needs each of these"
    )]
    fn resolve<'v>(
        &mut self,
        cx: &Ctx<'_>,
        parent: Parent<'v>,
        type_name: &str,
        def: &FieldDefinition,
        args: &Args,
        f: &Node<Field>,
        path: &Path<'_>,
        serde: &mut Option<Value>,
    ) -> Result<Out<'v>, GqlError> {
        let name = def.name.as_str();
        let obj = match parent {
            Parent::Edge(e) => {
                return match name {
                    "cursor" => Ok(leaf(e.cursor.clone())),
                    "node" => Ok(Out::R(e.node.clone())),
                    _ => Err(GqlError::new(
                        ErrorType::NotImplemented,
                        not_implemented(type_name, name),
                    )),
                };
            }
            Parent::PageInfo(p) => {
                return match name {
                    "hasNextPage" => Ok(leaf(p.has_next_page)),
                    "hasPreviousPage" => Ok(leaf(p.has_previous_page)),
                    "startCursor" => Ok(leaf(p.start_cursor.clone())),
                    "endCursor" => Ok(leaf(p.end_cursor.clone())),
                    _ => Err(GqlError::new(
                        ErrorType::NotImplemented,
                        not_implemented(type_name, name),
                    )),
                };
            }
            Parent::Obj(obj) => obj,
        };
        if let Some(r) = self.registry.resolver(type_name, name) {
            return r(cx, obj, args).map(Out::R);
        }
        if type_name == QUERY {
            match name {
                "node" => {
                    let id = args.get::<String>("id")?.unwrap_or_default();
                    return self.node(cx, &id).map(Out::R);
                }
                "nodes" => {
                    let ids = args.get::<Vec<String>>("ids")?.unwrap_or_default();
                    return Ok(Out::R(self.nodes(cx, &ids, f, path)));
                }
                _ => {}
            }
        }
        if let Some(conn) = (*obj.value).downcast_ref::<Conn>() {
            return match name {
                "edges" => Ok(Out::Edges(&conn.edges)),
                "nodes" => Ok(Out::Nodes(&conn.edges)),
                "pageInfo" => Ok(Out::PageInfo(&conn.page_info)),
                "totalCount" => Ok(leaf(conn.total_count)),
                _ => Err(GqlError::new(
                    ErrorType::NotImplemented,
                    not_implemented(type_name, name),
                )),
            };
        }
        if self.is_leaf(def.ty.inner_named_type()) {
            let form = serde.get_or_insert_with(|| {
                self.registry
                    .objects
                    .get(obj.type_name)
                    .and_then(|o| (o.serialize)(&*obj.value))
                    .unwrap_or(Value::Null)
            });
            if let Some(v) = form.get(name) {
                return Ok(Out::R(Resolved::Leaf(v.clone())));
            }
        }
        Err(GqlError::new(
            ErrorType::NotImplemented,
            not_implemented(type_name, name),
        ))
    }

    fn node(&mut self, cx: &Ctx<'_>, id: &str) -> Result<Resolved, GqlError> {
        let (r, next) = crate::load::node(cx, id)?;
        if let Some(next) = next {
            self.warnings.push(json!({
                "type": "DEPRECATION",
                "message": DEPRECATION.replacen("{}", id, 1),
                "data": {"legacy_global_id": id, "next_global_id": next},
                "link": "https://docs.github.com",
            }));
        }
        Ok(r)
    }

    fn nodes(
        &mut self,
        cx: &Ctx<'_>,
        ids: &[String],
        f: &Node<Field>,
        path: &Path<'_>,
    ) -> Resolved {
        let mut out = Vec::with_capacity(ids.len());
        for (i, id) in ids.iter().enumerate() {
            match self.node(cx, id) {
                Ok(r) => out.push(r),
                Err(e) => {
                    let at = Path {
                        parent: Some(path),
                        seg: Seg::Index(i),
                    };
                    self.error(e.kind, e.message, f, &at);
                    out.push(Resolved::Null);
                }
            }
        }
        Resolved::List(out)
    }

    fn is_leaf(&self, name: &str) -> bool {
        matches!(
            self.schema().types.get(name),
            Some(ExtendedType::Scalar(_) | ExtendedType::Enum(_))
        )
    }

    fn complete(
        &mut self,
        cx: &Ctx<'_>,
        parent_type: &str,
        ty: &Type,
        out: Out<'_>,
        fields: &Fields<'r>,
        path: &Path<'_>,
    ) -> Result<Value, Null> {
        let v = self.complete_nullable(cx, parent_type, ty, out, fields, path);
        if !ty.is_non_null() {
            return Ok(v.unwrap_or(Value::Null));
        }
        match v {
            Ok(Value::Null) => {
                let message = format!(
                    "Cannot return null for non-nullable field {parent_type}.{}",
                    fields[0].name
                );
                self.error(ErrorType::Validation, message, fields[0], path);
                Err(Null)
            }
            other => other,
        }
    }

    fn complete_nullable(
        &mut self,
        cx: &Ctx<'_>,
        parent_type: &str,
        ty: &Type,
        out: Out<'_>,
        fields: &Fields<'r>,
        path: &Path<'_>,
    ) -> Result<Value, Null> {
        if matches!(out, Out::R(Resolved::Null | Resolved::Leaf(Value::Null))) {
            return Ok(Value::Null);
        }
        if let Type::List(item) | Type::NonNullList(item) = ty {
            let items: Vec<Out<'_>> = match out {
                Out::R(Resolved::List(v)) => v.into_iter().map(Out::R).collect(),
                Out::R(Resolved::Leaf(Value::Array(v))) => {
                    v.into_iter().map(|x| Out::R(Resolved::Leaf(x))).collect()
                }
                Out::Edges(edges) => edges.iter().map(Out::Edge).collect(),
                Out::Nodes(edges) => edges.iter().map(|e| Out::R(e.node.clone())).collect(),
                _ => return self.internal("a list field resolved to a single value", fields, path),
            };
            let mut list = Vec::with_capacity(items.len());
            let mut nulled = false;
            for (i, out) in items.into_iter().enumerate() {
                let at = Path {
                    parent: Some(path),
                    seg: Seg::Index(i),
                };
                match self.complete(cx, parent_type, item, out, fields, &at) {
                    Ok(v) => list.push(v),
                    Err(Null) => nulled = true,
                }
            }
            return if nulled {
                Err(Null)
            } else {
                Ok(Value::Array(list))
            };
        }
        let named = ty.inner_named_type().as_str();
        if self.is_leaf(named) {
            return match out {
                Out::R(Resolved::Leaf(v)) => Ok(v),
                _ => self.internal("a scalar field resolved to an object", fields, path),
            };
        }
        match out {
            Out::Edge(e) => self.object(cx, Parent::Edge(e), named, fields, path),
            Out::PageInfo(p) => self.object(cx, Parent::PageInfo(p), named, fields, path),
            Out::R(Resolved::Object { type_name, value }) => {
                if let Err(message) = self.check(type_name, &value, named) {
                    return self.internal(&message, fields, path);
                }
                let obj = Obj { type_name, value };
                self.object(cx, Parent::Obj(&obj), type_name, fields, path)
            }
            _ => self.internal("an object field resolved to a scalar", fields, path),
        }
    }

    fn object(
        &mut self,
        cx: &Ctx<'_>,
        parent: Parent<'_>,
        type_name: &str,
        fields: &Fields<'r>,
        path: &Path<'_>,
    ) -> Result<Value, Null> {
        let mut groups = Groups::default();
        let mut visited = Vec::new();
        for f in fields {
            self.collect(type_name, &f.selection_set, &mut groups, &mut visited);
        }
        self.set(cx, parent, type_name, &groups, Some(path))
            .map(Value::Object)
    }

    fn check(
        &self,
        type_name: &str,
        value: &Arc<dyn Any + Send + Sync>,
        expected: &str,
    ) -> Result<(), String> {
        if self.schema().get_object(type_name).is_none() || !self.applies(type_name, expected) {
            return Err(format!("{type_name} is not a possible type of {expected}"));
        }
        let actual = (**value).type_id();
        if actual == TypeId::of::<Conn>() {
            return Ok(());
        }
        let Some(o) = self.registry.objects.get(type_name) else {
            return Err(format!("{type_name} has no registered Rust value"));
        };
        debug_assert!(
            o.type_id == actual,
            "{type_name} resolved to a value that is not a {}",
            o.rust_name
        );
        if o.type_id == actual {
            Ok(())
        } else {
            Err(format!(
                "{type_name} resolved to a value that is not a {}",
                o.rust_name
            ))
        }
    }

    fn internal(
        &mut self,
        message: &str,
        fields: &Fields<'r>,
        path: &Path<'_>,
    ) -> Result<Value, Null> {
        self.error(ErrorType::Internal, message.to_owned(), fields[0], path);
        Err(Null)
    }

    fn field_error(&mut self, e: GqlError, f: &Node<Field>, path: &Path<'_>) {
        let message = match e.kind {
            ErrorType::MissingPaginationBoundaries | ErrorType::ExcessivePagination => e
                .message
                .replacen("the connection", &format!("the `{}` connection", f.name), 1),
            _ => e.message,
        };
        self.error(e.kind, message, f, path);
    }

    fn error(&mut self, kind: ErrorType, message: String, f: &Node<Field>, path: &Path<'_>) {
        let locations = locations(f.location(), self.doc);
        self.errors.push(ResponseError {
            kind,
            message,
            path: path.segments(),
            locations,
        });
    }

    fn args(&self, f: &Field, def: &FieldDefinition) -> Args {
        let mut map = Map::new();
        for arg in &def.arguments {
            let given = f
                .arguments
                .iter()
                .find(|a| a.name == arg.name)
                .and_then(|a| self.json(&a.value));
            let value = given.or_else(|| arg.default_value.as_ref().and_then(|d| self.json(d)));
            if let Some(v) = value {
                let v = match v {
                    Value::Null | Value::Array(_) => v,
                    single if arg.ty.is_list() => Value::Array(vec![single]),
                    other => other,
                };
                map.insert(arg.name.to_string(), v);
            }
        }
        Args(map)
    }

    fn json(&self, v: &Ast) -> Option<Value> {
        Some(match v {
            Ast::Null => Value::Null,
            Ast::Enum(n) => Value::String(n.to_string()),
            Ast::Variable(n) => return self.vars.get(n.as_str()).cloned(),
            Ast::String(s) => Value::String(s.clone()),
            Ast::Float(x) => x
                .try_to_f64()
                .ok()
                .and_then(serde_json::Number::from_f64)
                .map_or(Value::Null, Value::Number),
            Ast::Int(i) => i.as_str().parse::<i64>().map_or(Value::Null, Value::from),
            Ast::Boolean(b) => Value::Bool(*b),
            Ast::List(items) => Value::Array(
                items
                    .iter()
                    .map(|i| self.json(i).unwrap_or(Value::Null))
                    .collect(),
            ),
            Ast::Object(fields) => Value::Object(
                fields
                    .iter()
                    .filter_map(|(k, v)| Some((k.to_string(), self.json(v)?)))
                    .collect(),
            ),
        })
    }
}

fn leaf(v: impl Into<Value>) -> Out<'static> {
    Out::R(Resolved::Leaf(v.into()))
}

fn nulled(ty: &Type) -> Result<Value, Null> {
    if ty.is_non_null() {
        Err(Null)
    } else {
        Ok(Value::Null)
    }
}

fn not_implemented(type_name: &str, field: &str) -> String {
    format!("hubdouble does not implement {type_name}.{field} yet")
}
