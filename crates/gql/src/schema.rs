use apollo_compiler::schema::ExtendedType;
use apollo_compiler::validation::Valid;

use crate::contract::{ErrorType, GqlError, ResponseError};

/// GitHub's parsed GraphQL schema, loaded once per server.
#[derive(Debug)]
pub struct Schema {
    pub(crate) inner: Valid<apollo_compiler::Schema>,
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
            .map(|inner| Schema { inner })
            .map_err(|e| GqlError::new(ErrorType::Internal, e.errors.to_string()))
    }

    /// Validates a document with GitHub's rule set (plan 4.7).
    ///
    /// # Errors
    ///
    /// The validation errors, shaped as GitHub answers them.
    pub fn check(&self, query: &str) -> Result<(), Vec<ResponseError>> {
        crate::validate::validate(self, query).map(drop)
    }

    #[must_use]
    pub fn type_count(&self) -> usize {
        self.inner.types.len()
    }

    pub(crate) fn is_object(&self, name: &str) -> bool {
        matches!(self.inner.types.get(name), Some(ExtendedType::Object(_)))
    }
}
