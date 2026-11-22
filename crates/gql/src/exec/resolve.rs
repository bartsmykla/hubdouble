use apollo_compiler::Node;
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
}
