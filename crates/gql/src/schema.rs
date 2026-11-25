use std::collections::HashMap;
use std::sync::OnceLock;

use apollo_compiler::executable::{Selection, SelectionSet};
use apollo_compiler::schema::ExtendedType;
use apollo_compiler::validation::Valid;
use bytes::Bytes;

use crate::contract::{ErrorType, GqlError, ResponseError};
use crate::leaf::Leaf;
use crate::meta::Meta;

/// GitHub's parsed GraphQL schema, loaded once per server.
#[derive(Debug)]
pub struct Schema {
    pub(crate) inner: Valid<apollo_compiler::Schema>,
    leaves: HashMap<String, Leaf>,
    meta: OnceLock<Result<Meta, GqlError>>,
}

impl Schema {
    /// Parses and validates the schema that `hubdouble-spec` embeds.
    ///
    /// # Errors
    ///
    /// An `INTERNAL` error when the embedded schema does not parse or validate.
    pub fn load() -> Result<Schema, GqlError> {
        Schema::parse(hubdouble_spec::GRAPHQL_SCHEMA)
    }

    /// Parses and validates a schema from SDL text.
    ///
    /// # Errors
    ///
    /// An `INTERNAL` error when the text does not parse or validate.
    pub fn parse(sdl: &str) -> Result<Schema, GqlError> {
        apollo_compiler::Schema::parse_and_validate(sdl, "schema.graphql")
            .map(|inner| Schema {
                leaves: crate::leaf::leaves(&inner),
                inner,
                meta: OnceLock::new(),
            })
            .map_err(|e| GqlError::new(ErrorType::Internal, e.errors.to_string()))
    }

    /// The body that live `GET /graphql` answered, byte for byte.
    #[must_use]
    pub fn introspection(&self) -> Bytes {
        Bytes::from_static(hubdouble_spec::GRAPHQL_INTROSPECTION.trim_end().as_bytes())
    }

    pub(crate) fn meta(&self) -> Result<&Meta, &GqlError> {
        self.meta
            .get_or_init(|| {
                Meta::load(
                    hubdouble_spec::GRAPHQL_INTROSPECTION,
                    hubdouble_spec::GRAPHQL_INTROSPECTION_EXTRA,
                )
            })
            .as_ref()
    }

    /// Validates a document with GitHub's rule set (plan 4.7).
    ///
    /// # Errors
    ///
    /// The validation errors, shaped as GitHub answers them.
    pub fn check(&self, query: &str) -> Result<(), Vec<ResponseError>> {
        crate::validate::validate(self, query).map(drop)
    }

    /// Every `(parent type, field)` a document selects, sorted and without duplicates; the parent
    /// is the type of the selection set the field is written in, and meta fields are left out.
    ///
    /// # Errors
    ///
    /// The validation errors of a document that does not validate.
    pub fn selected_fields(
        &self,
        query: &str,
    ) -> Result<Vec<(String, String)>, Vec<ResponseError>> {
        let doc = crate::validate::validate(self, query)?;
        let mut out = Vec::new();
        for op in doc.operations.iter() {
            walk(&op.selection_set, &mut out);
        }
        for fragment in doc.fragments.values() {
            walk(&fragment.selection_set, &mut out);
        }
        out.sort();
        out.dedup();
        Ok(out)
    }

    #[must_use]
    pub fn type_count(&self) -> usize {
        self.inner.types.len()
    }

    pub(crate) fn leaf(&self, name: &str) -> Option<&Leaf> {
        self.leaves.get(name)
    }

    pub(crate) fn is_object(&self, name: &str) -> bool {
        matches!(self.inner.types.get(name), Some(ExtendedType::Object(_)))
    }
}

fn walk(set: &SelectionSet, out: &mut Vec<(String, String)>) {
    for selection in &set.selections {
        match selection {
            Selection::Field(f) => {
                if !f.name.starts_with("__") {
                    out.push((set.ty.to_string(), f.name.to_string()));
                }
                walk(&f.selection_set, out);
            }
            Selection::InlineFragment(inline) => walk(&inline.selection_set, out),
            Selection::FragmentSpread(_) => {}
        }
    }
}
