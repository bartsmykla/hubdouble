mod complete;
mod request;
mod resolve;

use apollo_compiler::ast::{Type, Value as Ast};
use apollo_compiler::collections::IndexMap;
use apollo_compiler::executable::{Field, Selection, SelectionSet};
use apollo_compiler::schema::FieldDefinition;
use apollo_compiler::{ExecutableDocument, Node};
use hubdouble_auth::Actor;
use hubdouble_git::Git;
use hubdouble_model::ApiError;
use hubdouble_store::Store;
use serde_json::{Map, Value, json};

use crate::connection::CursorError;
use crate::contract::{
    Args, Ctx, Edge, ErrorType, GqlError, MutCtx, Obj, PageInfo, Registry, Request, Resolved,
    ResponseError,
};
use crate::request_errors::locations;
use crate::selection::included;

pub(crate) use request::{execute, variables};

const QUERY: &str = "Query";
const MUTATION: &str = "Mutation";
const DEPRECATION: &str =
    "The id {} is deprecated. Update your cache to use the next_global_id from the data payload.";

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
    Unpaged(&'v Obj),
    InvalidConnection(&'v Obj, &'v CursorError),
    Edge(&'v Edge),
    PageInfo(&'v PageInfo),
    InvalidPageInfo(&'v GqlError),
}

enum Out<'v> {
    R(Resolved),
    Edges(&'v [Edge]),
    Nodes(&'v [Edge]),
    Edge(&'v Edge),
    PageInfo(&'v PageInfo),
    InvalidPageInfo(&'v GqlError),
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
                    if included(&f.directives, self.vars) {
                        groups.entry(f.response_key().as_str()).or_default().push(f);
                    }
                }
                Selection::InlineFragment(inline) => {
                    if included(&inline.directives, self.vars)
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
                    if included(&spread.directives, self.vars) && !visited.contains(&name) {
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

    fn applies(&self, object: &str, condition: &str) -> bool {
        object == condition || self.schema().is_subtype(condition, object)
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
                Err(Null) if matches!(parent, Parent::InvalidPageInfo(_)) => return Err(Null),
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

    fn field_error(&mut self, e: GqlError, f: &Node<Field>, path: &Path<'_>) {
        let message = match e.kind {
            ErrorType::MissingPaginationBoundaries
            | ErrorType::ExcessivePagination
            | ErrorType::InvalidPagination
            | ErrorType::Validation => {
                e.message
                    .replacen("the connection", &format!("the `{}` connection", f.name), 1)
            }
            _ => e.message,
        };
        self.error(e.kind, message, f, path);
    }

    fn error(&mut self, kind: ErrorType, message: String, f: &Node<Field>, path: &Path<'_>) {
        let locations = locations(f.location(), &self.doc.sources);
        self.errors.push(ResponseError {
            kind,
            message,
            path: path.segments(),
            locations,
            extensions: None,
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
