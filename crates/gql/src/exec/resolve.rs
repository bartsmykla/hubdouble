use apollo_compiler::Node;
use apollo_compiler::ast::Value as Ast;
use apollo_compiler::executable::Field;
use apollo_compiler::schema::FieldDefinition;
use serde_json::Value;

use crate::connection::CursorErrorFields;
use crate::contract::{Args, Conn, Ctx, ErrorType, GqlError, Resolved};

use super::{Exec, Out, Parent, Path, QUERY, leaf, not_implemented};

impl Exec<'_> {
    #[expect(
        clippy::too_many_arguments,
        reason = "one resolution step needs each of these"
    )]
    pub(super) fn resolve<'v>(
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
        if self.fails_internally(type_name, name, f, args) {
            self.failed_internally = true;
            return Err(GqlError::new(ErrorType::Internal, String::new()));
        }
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
            Parent::InvalidPageInfo(error) => return Err(error.clone()),
            Parent::InvalidConnection(obj, cursor) => match (name, cursor.fields) {
                ("nodes", _)
                | ("edges" | "pageInfo", CursorErrorFields::Page)
                | ("edges", CursorErrorFields::PageInfoFields) => return Err(cursor.error.clone()),
                ("pageInfo", CursorErrorFields::PageInfoFields) => {
                    return Ok(Out::InvalidPageInfo(&cursor.error));
                }
                _ => obj,
            },
            Parent::Obj(obj) | Parent::Unpaged(obj) => obj,
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
        if let Some(leaf) = self.registry.schema.leaf(def.ty.inner_named_type()) {
            let form = serde.get_or_insert_with(|| {
                self.registry
                    .objects
                    .get(obj.type_name)
                    .and_then(|o| (o.serialize)(&*obj.value))
                    .unwrap_or(Value::Null)
            });
            if let Some(v) = form.get(name)
                && leaf.fits(&def.ty, v)
            {
                return Ok(Out::R(Resolved::Leaf(v.clone())));
            }
        }
        Err(GqlError::new(
            ErrorType::NotImplemented,
            not_implemented(type_name, name),
        ))
    }

    fn fails_internally(&self, type_name: &str, field: &str, f: &Field, args: &Args) -> bool {
        f.arguments.iter().any(|arg| {
            let name = arg.name.as_str();
            let repository = type_name == QUERY && field == "repository";
            let reads_text = matches!(
                (type_name, field, name),
                (QUERY, "repository", "owner" | "name")
                    | (QUERY, "organization", "login")
                    | ("Repository", "refs", "query")
                    | ("Repository", "collaborators", "login")
                    | (_, _, "after" | "before")
            );
            let lone_low = reads_text
                && matches!(&*arg.value, Ast::Variable(v) if self.lone_low_vars.iter().any(|l| l == v.as_str()));
            let astral = repository
                && matches!(name, "owner" | "name")
                && args
                    .0
                    .get(name)
                    .and_then(Value::as_str)
                    .is_some_and(|text| text.chars().any(|c| c > '\u{FFFF}'));
            lone_low || astral
        })
    }
}
