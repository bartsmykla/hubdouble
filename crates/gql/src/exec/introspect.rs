use apollo_compiler::Node;
use apollo_compiler::executable::{Field, SelectionSet};
use serde_json::{Map, Value};

use super::{Exec, Fields, Groups, Null, Object, Path};
use crate::contract::ErrorType;
use crate::data::Data;
use crate::meta::{M, Meta, R};

impl<'r> Exec<'r> {
    pub(super) fn introspect(
        &mut self,
        fields: &Fields<'r>,
        path: &Path<'_>,
    ) -> Result<Data, Null> {
        let f = fields[0];
        let meta = match self.registry.schema.meta() {
            Ok(meta) => meta,
            Err(e) => {
                self.error(ErrorType::Internal, e.message.clone(), f, path);
                return Err(Null);
            }
        };
        let args = self.field_args(f);
        let root = if f.name == "__schema" {
            Some(M::Schema)
        } else {
            args.get("name")
                .and_then(Value::as_str)
                .and_then(|name| meta.type_by_name(name))
        };
        Ok(match root {
            Some(m) => Data::Object(self.meta_set(meta, m, fields)),
            None => Data::NULL,
        })
    }

    fn field_args(&self, f: &Field) -> Map<String, Value> {
        f.arguments
            .iter()
            .filter_map(|a| Some((a.name.to_string(), self.json(&a.value)?)))
            .collect()
    }

    fn meta_set(&mut self, meta: &Meta, m: M<'_>, fields: &Fields<'r>) -> Object {
        let type_name = m.type_name();
        let mut groups = Groups::default();
        for f in fields {
            let set: &'r SelectionSet = &f.selection_set;
            self.collect(type_name, set, &mut groups, &mut Vec::new());
        }
        let mut out = Vec::with_capacity(groups.len());
        for (key, group) in &groups {
            let f: &Node<Field> = group[0];
            let value = if f.name == "__typename" {
                Data::Leaf(Value::String(type_name.to_owned()))
            } else {
                let args = self.field_args(f);
                match meta.get(m, f.name.as_str(), &args) {
                    R::Leaf(v) => Data::Leaf(v),
                    R::One(None) | R::Many(None) => Data::NULL,
                    R::One(Some(inner)) => Data::Object(self.meta_set(meta, inner, group)),
                    R::Many(Some(items)) => Data::List(
                        items
                            .into_iter()
                            .map(|inner| Data::Object(self.meta_set(meta, inner, group)))
                            .collect(),
                    ),
                }
            };
            out.push(((*key).to_owned(), value));
        }
        out
    }
}
