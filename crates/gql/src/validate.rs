use std::collections::HashSet;

use apollo_compiler::ast::{Document, Type};
use apollo_compiler::diagnostic::{Diagnostic, ToCliReport};
use apollo_compiler::executable::{Field, Selection, SelectionSet};
use apollo_compiler::parser::SourceSpan;
use apollo_compiler::validation::{DiagnosticData, DiagnosticList, Valid};
use apollo_compiler::{ExecutableDocument, Node};

use crate::contract::{ErrorType, ResponseError};
use crate::schema::Schema;

const CONFLICTING_FIELD_TYPE: &str = "ConflictingFieldType";

pub(crate) fn validate(
    schema: &Schema,
    query: &str,
) -> Result<Valid<ExecutableDocument>, Vec<ResponseError>> {
    let ast = Document::parse(query, "query.graphql").map_err(|e| errors(&e.errors))?;
    let doc = ast.to_executable(&schema.inner).map_err(|e| {
        e.errors
            .iter()
            .map(|d| crate::request_errors::validation(schema, &ast, &d, error(&d)))
            .collect::<Vec<_>>()
    })?;
    match doc.validate(&schema.inner) {
        Ok(valid) => Ok(valid),
        Err(e) => {
            let kept: Vec<ResponseError> = e
                .errors
                .iter()
                .filter(|d| !tolerated(schema, &e.partial, d))
                .map(|d| crate::request_errors::validation(schema, &ast, &d, error(&d)))
                .collect();
            if kept.is_empty() {
                Ok(Valid::assume_valid(e.partial))
            } else {
                Err(kept)
            }
        }
    }
}

fn errors(list: &DiagnosticList) -> Vec<ResponseError> {
    list.iter().map(|d| error(&d)).collect()
}

pub(crate) fn error(d: &Diagnostic<'_, DiagnosticData>) -> ResponseError {
    ResponseError {
        kind: ErrorType::Validation,
        message: d.error.to_string(),
        path: Vec::new(),
        locations: d.line_column_range().map_or_else(Vec::new, |r| {
            vec![serde_json::json!({"line": r.start.line, "column": r.start.column})]
        }),
        extensions: None,
    }
}

type Groups<'a> = Vec<(&'a str, Vec<(&'a str, &'a Node<Field>)>)>;

const MAX_DEPTH: usize = 64;

fn tolerated(
    schema: &Schema,
    doc: &ExecutableDocument,
    d: &Diagnostic<'_, DiagnosticData>,
) -> bool {
    if d.error.unstable_error_name() != Some(CONFLICTING_FIELD_TYPE) {
        return false;
    }
    let Some(at) = d.error.location() else {
        return false;
    };
    let mut check = Recheck {
        schema,
        doc,
        at,
        found: false,
        conflict: false,
    };
    for op in doc.operations.iter() {
        check.merged(&[&op.selection_set], 0);
    }
    for fragment in doc.fragments.values() {
        check.merged(&[&fragment.selection_set], 0);
    }
    check.found && !check.conflict
}

struct Recheck<'a> {
    schema: &'a Schema,
    doc: &'a ExecutableDocument,
    at: SourceSpan,
    found: bool,
    conflict: bool,
}

impl<'a> Recheck<'a> {
    fn merged(&mut self, sets: &[&'a SelectionSet], depth: usize) {
        if depth > MAX_DEPTH {
            self.conflict = true;
            return;
        }
        let mut groups: Groups<'a> = Vec::new();
        let mut seen = HashSet::new();
        for set in sets {
            self.collect(set, &mut groups, &mut seen);
        }
        for (_, fields) in &groups {
            if let Some(&(parent, field)) =
                fields.iter().find(|(_, f)| f.location() == Some(self.at))
            {
                self.found = true;
                for &(other_parent, other) in fields {
                    if !std::ptr::eq(other, field)
                        && !self.compatible(
                            parent,
                            &field.definition.ty,
                            other_parent,
                            &other.definition.ty,
                        )
                    {
                        self.conflict = true;
                    }
                }
            }
            let children: Vec<&'a SelectionSet> = fields
                .iter()
                .map(|(_, f)| &f.selection_set)
                .filter(|s| !s.selections.is_empty())
                .collect();
            if !children.is_empty() {
                self.merged(&children, depth + 1);
            }
        }
    }

    fn collect(&self, set: &'a SelectionSet, groups: &mut Groups<'a>, seen: &mut HashSet<&'a str>) {
        for selection in &set.selections {
            match selection {
                Selection::Field(f) => {
                    let key = f.response_key().as_str();
                    let entry = (set.ty.as_str(), f);
                    match groups.iter_mut().find(|(k, _)| *k == key) {
                        Some((_, fields)) => fields.push(entry),
                        None => groups.push((key, vec![entry])),
                    }
                }
                Selection::InlineFragment(inline) => {
                    self.collect(&inline.selection_set, groups, seen);
                }
                Selection::FragmentSpread(spread) => {
                    let name = spread.fragment_name.as_str();
                    if seen.insert(name)
                        && let Some(fragment) = self.doc.fragments.get(name)
                    {
                        self.collect(&fragment.selection_set, groups, seen);
                    }
                }
            }
        }
    }

    fn compatible(&self, parent_a: &str, a: &Type, parent_b: &str, b: &Type) -> bool {
        a == b
            || (parent_a != parent_b
                && self.schema.is_object(parent_a)
                && self.schema.is_object(parent_b)
                && nullable(a) == nullable(b))
    }
}

fn nullable(t: &Type) -> Type {
    match t {
        Type::Named(n) | Type::NonNullNamed(n) => Type::Named(n.clone()),
        Type::List(inner) | Type::NonNullList(inner) => Type::List(Box::new(nullable(inner))),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::LazyLock;

    use super::*;

    static SCHEMA: LazyLock<Schema> = LazyLock::new(|| match Schema::load() {
        Ok(s) => s,
        Err(e) => panic!("{e}"),
    });

    fn kinds(query: &str) -> Vec<Option<&'static str>> {
        let doc = match ExecutableDocument::parse(&SCHEMA.inner, query, "q.graphql") {
            Ok(d) => d,
            Err(e) => panic!("{}", e.errors),
        };
        match doc.validate(&SCHEMA.inner) {
            Ok(_) => Vec::new(),
            Err(e) => e
                .errors
                .iter()
                .map(|d| d.error.unstable_error_name())
                .collect(),
        }
    }

    const ORIGINAL_COMMENT_EDIT: &str =
        include_str!("../../../docs/inventory/graphql/originalCommentEdit.graphql");

    #[test]
    fn apollo_reports_the_nullability_merge_as_a_conflicting_field_type() {
        // Given the client's originalCommentEdit, which GitHub accepted on 2026-11-22
        // When apollo-compiler runs the spec's full validation
        // Then it reports exactly one diagnostic, narrowable by its kind
        assert_eq!(
            kinds(ORIGINAL_COMMENT_EDIT),
            vec![Some(CONFLICTING_FIELD_TYPE)]
        );
    }

    #[test]
    fn the_schema_is_valid_and_loads() {
        assert!(SCHEMA.type_count() > 1000);
        assert!(SCHEMA.is_object("PullRequest"));
        assert!(!SCHEMA.is_object("Comment"));
    }

    #[test]
    fn accepts_nullability_differences_between_distinct_concrete_types() {
        // Given the document GitHub accepts
        // When it is validated with GitHub's rule set
        // Then it is valid
        assert!(validate(&SCHEMA, ORIGINAL_COMMENT_EDIT).is_ok());
    }

    fn rejected(query: &str) -> Vec<ResponseError> {
        match validate(&SCHEMA, query) {
            Ok(_) => panic!("accepted {query}"),
            Err(e) => e,
        }
    }

    #[test]
    fn keeps_a_conflict_of_different_named_types() {
        let query = "query($id: ID!) { node(id: $id) { \
             ... on IssueComment { x: body } \
             ... on PullRequestReviewComment { x: databaseId } } }";
        let column = query.find("x: databaseId").map(|c| c + 1);
        let e = rejected(query);
        assert_eq!(e.len(), 1);
        assert_eq!(e[0].kind, ErrorType::Validation);
        assert_eq!(
            e[0].locations,
            vec![serde_json::json!({"line": 1, "column": column})]
        );
    }

    #[test]
    fn keeps_a_nullability_conflict_when_a_parent_is_an_interface() {
        // Given AuditEntry.user: User (an interface) and DeploymentReview.user: User!
        // When both are selected into one response name
        // Then the conflict stays an error, because the parents are not two concrete types
        rejected(
            "query($id: ID!) { node(id: $id) { \
             ... on AuditEntry { x: user { login } } \
             ... on DeploymentReview { x: user { login } } } }",
        );
    }

    #[test]
    fn keeps_a_real_conflict_nested_under_a_tolerated_one() {
        // Given a tolerated PullRequest / PullRequest! pair whose subselections conflict
        // When the document is validated
        // Then the nested Int! / String! conflict stays an error
        let e = rejected(
            "query($id: ID!) { node(id: $id) { \
             ... on IssueComment { pullRequest { n: number } } \
             ... on PullRequestReviewComment { pullRequest { n: title } } } }",
        );
        assert_eq!(e.len(), 1);
    }

    #[test]
    fn keeps_every_other_kind_of_error() {
        let e = rejected("{ viewer { nope } }");
        assert_eq!(e.len(), 1);
        assert_eq!(e[0].kind, ErrorType::Validation);
        let e = rejected("{ viewer { login ");
        assert_ne!(e.len(), 0);
        let e = rejected(
            "query($id: ID!) { node(id: $id) { \
             ... on IssueComment { x: body } ... on IssueComment { x: bodyText } } }",
        );
        assert_eq!(e.len(), 1);
    }

    #[test]
    fn every_inventory_document_validates() {
        let docs = crate::inventory::documents();
        assert_eq!(docs.len(), 30);
        for (name, text) in &docs {
            if let Err(e) = validate(&SCHEMA, text) {
                panic!("{name}: {e:?}");
            }
        }
    }
}
