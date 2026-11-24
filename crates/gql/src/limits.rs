use std::collections::{HashMap, HashSet};

use apollo_compiler::ast::Value as Ast;
use apollo_compiler::collections::IndexMap;
use apollo_compiler::executable::{Field, Operation, Selection, SelectionSet};
use apollo_compiler::parser::SourceSpan;
use apollo_compiler::{ExecutableDocument, Node};
use serde_json::{Map, Value};

use crate::connection::MAX_PAGE;
use crate::contract::{ErrorType, ResponseError};
use crate::request_errors::locations;
use crate::selection::included;

const MAX_DEPTH: usize = 25;
const MAX_NODES: usize = 500_000;

#[expect(
    clippy::result_large_err,
    reason = "the error is the response error the request answers at once, built once per request"
)]
pub(crate) fn check(
    doc: &ExecutableDocument,
    op: &Operation,
    vars: &Map<String, Value>,
) -> Result<(), ResponseError> {
    let depth = depth(doc, &op.selection_set, vars, &mut HashMap::new());
    if depth > MAX_DEPTH {
        return Err(ResponseError {
            kind: ErrorType::Validation,
            message: format!("Query has depth of {depth}, which exceeds max depth of {MAX_DEPTH}"),
            path: Vec::new(),
            locations: Vec::new(),
            extensions: None,
        });
    }
    let cost = Cost { doc, vars }.count(&[&op.selection_set], 1, None, &mut HashMap::new());
    if cost.total <= MAX_NODES {
        return Ok(());
    }
    let (message, at) = match cost.over {
        Some((name, count, at)) => (
            format!(
                "By the time this query traverses to the {name} connection, it is requesting up to {} possible nodes which exceeds the maximum limit of {}.",
                separated(count),
                separated(MAX_NODES),
            ),
            at,
        ),
        None => (
            format!(
                "This query requests up to {} possible nodes which exceeds the maximum limit of {}.",
                separated(cost.total),
                separated(MAX_NODES),
            ),
            None,
        ),
    };
    Err(ResponseError {
        kind: ErrorType::MaxNodeLimitExceeded,
        message,
        path: Vec::new(),
        locations: locations(at, &doc.sources),
        extensions: None,
    })
}

fn separated(value: usize) -> String {
    let digits = value.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, ch) in digits.chars().enumerate() {
        if i != 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

fn depth<'a>(
    doc: &'a ExecutableDocument,
    set: &'a SelectionSet,
    vars: &Map<String, Value>,
    fragments: &mut HashMap<&'a str, usize>,
) -> usize {
    set.selections
        .iter()
        .map(|selection| match selection {
            Selection::Field(f) if included(&f.directives, vars) => {
                1 + depth(doc, &f.selection_set, vars, fragments)
            }
            Selection::InlineFragment(f) if included(&f.directives, vars) => {
                depth(doc, &f.selection_set, vars, fragments)
            }
            Selection::FragmentSpread(f) if included(&f.directives, vars) => {
                let name = f.fragment_name.as_str();
                if let Some(&cached) = fragments.get(name) {
                    return cached;
                }
                let n = doc
                    .fragments
                    .get(name)
                    .map_or(0, |f| depth(doc, &f.selection_set, vars, fragments));
                fragments.insert(name, n);
                n
            }
            _ => 0,
        })
        .max()
        .unwrap_or(0)
}

type Fields<'a> = IndexMap<&'a str, Vec<&'a Node<Field>>>;
type Key = (Vec<usize>, usize, Option<usize>);

#[derive(Clone, Copy, Default)]
struct Estimate<'a> {
    total: usize,
    over: Option<(&'a str, usize, Option<SourceSpan>)>,
}

struct Cost<'a> {
    doc: &'a ExecutableDocument,
    vars: &'a Map<String, Value>,
}

impl<'a> Cost<'a> {
    fn count(
        &self,
        sets: &[&'a SelectionSet],
        parents: usize,
        connection: Option<usize>,
        memo: &mut HashMap<Key, Estimate<'a>>,
    ) -> Estimate<'a> {
        if parents == 0 || sets.iter().all(|set| set.selections.is_empty()) {
            return Estimate::default();
        }
        let key = (
            sets.iter().map(|s| std::ptr::from_ref(*s).addr()).collect(),
            parents,
            connection,
        );
        if let Some(&estimate) = memo.get(&key) {
            return estimate;
        }
        let mut fields = Fields::default();
        let mut visited = HashSet::new();
        for set in sets {
            self.collect(set, &mut fields, &mut visited);
        }
        let mut estimate = Estimate::default();
        for group in fields.values() {
            let (field, size) = group
                .iter()
                .map(|f| (*f, self.page_size(f)))
                .max_by_key(|(_, size)| *size)
                .unwrap_or((group[0], None));
            let name = field.name.as_str();
            let next_parents = if matches!(name, "nodes" | "edges") {
                parents.saturating_mul(connection.unwrap_or(1))
            } else {
                parents
            };
            let nodes = next_parents.saturating_mul(size.unwrap_or(0));
            if nodes > MAX_NODES {
                return Estimate {
                    total: nodes,
                    over: Some((name, nodes, field.location())),
                };
            }
            let children: Vec<_> = group.iter().map(|f| &f.selection_set).collect();
            let inner = self.count(&children, next_parents, size, memo);
            if inner.over.is_some() {
                return inner;
            }
            estimate.total = estimate
                .total
                .saturating_add(nodes.saturating_add(inner.total));
        }
        memo.insert(key, estimate);
        estimate
    }

    fn page_size(&self, field: &Field) -> Option<usize> {
        if !field
            .definition
            .ty
            .inner_named_type()
            .ends_with("Connection")
        {
            return None;
        }
        ["first", "last"].into_iter().find_map(|page| {
            let value = field.specified_argument_by_name(page)?;
            let size = match value.as_ref() {
                Ast::Int(n) => n.as_str().parse::<usize>().ok(),
                Ast::Variable(n) => self
                    .vars
                    .get(n.as_str())
                    .and_then(Value::as_u64)
                    .and_then(|n| usize::try_from(n).ok()),
                _ => None,
            }?;
            (size <= MAX_PAGE).then_some(size)
        })
    }

    fn collect(
        &self,
        set: &'a SelectionSet,
        fields: &mut Fields<'a>,
        visited: &mut HashSet<&'a str>,
    ) {
        for selection in &set.selections {
            match selection {
                Selection::Field(f) if included(&f.directives, self.vars) => {
                    fields.entry(f.response_key().as_str()).or_default().push(f);
                }
                Selection::InlineFragment(f) if included(&f.directives, self.vars) => {
                    self.collect(&f.selection_set, fields, visited);
                }
                Selection::FragmentSpread(f)
                    if included(&f.directives, self.vars)
                        && visited.insert(f.fragment_name.as_str()) =>
                {
                    if let Some(fragment) = self.doc.fragments.get(&f.fragment_name) {
                        self.collect(&fragment.selection_set, fields, visited);
                    }
                }
                _ => {}
            }
        }
    }
}
