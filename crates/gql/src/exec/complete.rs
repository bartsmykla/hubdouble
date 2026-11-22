use std::any::{Any, TypeId};
use std::collections::HashSet;
use std::sync::Arc;

use apollo_compiler::ast::Type;
use apollo_compiler::executable::{Selection, SelectionSet};
use apollo_compiler::schema::ExtendedType;
use serde_json::Value;

use super::{Exec, Fields, Groups, Null, Out, Parent, Path, Seg};
use crate::connection::{IncompletePage, PartialConnection};
use crate::contract::{Conn, Ctx, ErrorType, GqlError, Obj, Resolved};

impl<'r> Exec<'r> {
    pub(super) fn is_leaf(&self, name: &str) -> bool {
        matches!(
            self.schema().types.get(name),
            Some(ExtendedType::Scalar(_) | ExtendedType::Enum(_))
        )
    }

    pub(super) fn complete(
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
            Out::InvalidPageInfo(cursor) => {
                self.object(cx, Parent::InvalidPageInfo(cursor), named, fields, path)
            }
            Out::R(Resolved::Object { type_name, value }) => {
                if let Err(message) = self.check(type_name, &value, named) {
                    return self.internal(&message, fields, path);
                }
                if let Some(partial) = value.downcast_ref::<PartialConnection>() {
                    let obj = Obj {
                        type_name,
                        value: partial.connection.clone(),
                    };
                    let parent = match &partial.page {
                        IncompletePage::MissingSize => Parent::Unpaged(&obj),
                        IncompletePage::InvalidCursor(cursor) => {
                            Parent::InvalidConnection(&obj, cursor)
                        }
                    };
                    return self.object(cx, parent, type_name, fields, path);
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
        if matches!(parent, Parent::Unpaged(_)) {
            let mut visited = HashSet::new();
            if fields
                .iter()
                .any(|f| self.selects_page_items(&f.selection_set, &mut visited))
            {
                self.field_error(GqlError::new(
                    ErrorType::MissingPaginationBoundaries,
                    "You must provide a `first` or `last` value to properly paginate the connection.",
                ), fields[0], path);
                return Err(Null);
            }
        }
        let mut groups = Groups::default();
        let mut visited = Vec::new();
        for f in fields {
            self.collect(type_name, &f.selection_set, &mut groups, &mut visited);
        }
        self.set(cx, parent, type_name, &groups, Some(path))
            .map(Value::Object)
    }

    fn selects_page_items(&self, set: &'r SelectionSet, visited: &mut HashSet<&'r str>) -> bool {
        set.selections.iter().any(|part| match part {
            Selection::Field(field) => matches!(field.name.as_str(), "nodes" | "edges"),
            Selection::InlineFragment(fragment) => {
                self.selects_page_items(&fragment.selection_set, visited)
            }
            Selection::FragmentSpread(spread) => {
                visited.insert(spread.fragment_name.as_str())
                    && self
                        .doc
                        .fragments
                        .get(&spread.fragment_name)
                        .is_some_and(|fragment| {
                            self.selects_page_items(&fragment.selection_set, visited)
                        })
            }
        })
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
        if actual == TypeId::of::<Conn>() || actual == TypeId::of::<PartialConnection>() {
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
}
