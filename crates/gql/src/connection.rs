use std::sync::Arc;

use serde::Serialize;

use crate::contract::{Args, ErrorType, GqlError, Resolved};

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PageInfo {
    pub has_next_page: bool,
    pub has_previous_page: bool,
    pub start_cursor: Option<String>,
    pub end_cursor: Option<String>,
}

/// An edge of a [`Conn`].
#[derive(Clone)]
pub struct Edge {
    pub cursor: String,
    pub node: Resolved,
}

/// The value behind every connection type P0-B owns.
#[derive(Clone)]
pub struct Conn {
    pub type_name: &'static str,
    pub edges: Vec<Edge>,
    pub page_info: PageInfo,
    pub total_count: usize,
}

/// Slices `items` by `first`, `last`, `after` and `before` into a [`Conn`] named `type_name`
/// (`PullRequestConnection`), an empty page included.
///
/// # Errors
///
/// GitHub's error for a connection without `first` or `last`, or with more than 100.
pub fn connection<T>(
    type_name: &'static str,
    items: &[T],
    args: &Args,
    cursor: impl Fn(&T) -> String,
    node: impl Fn(&T) -> Resolved,
) -> Result<Resolved, GqlError> {
    let first = page_size(args, "first")?;
    let last = page_size(args, "last")?;
    if first.is_some() && last.is_some() {
        return Err(GqlError::new(
            ErrorType::Validation,
            "Passing both `first` and `last` to paginate the connection is not supported.",
        ));
    }
    if first.is_none() && last.is_none() {
        return Err(GqlError::new(
            ErrorType::MissingPaginationBoundaries,
            "You must provide a `first` or `last` value to properly paginate the connection.",
        ));
    }
    let mut start = 0;
    let mut end = items.len();
    if let Some(after) = args.get::<String>("after")? {
        start = position(items, &cursor, &after, "after")? + 1;
    }
    if let Some(before) = args.get::<String>("before")? {
        end = position(items, &cursor, &before, "before")?;
    }
    end = end.max(start);
    if let Some(first) = first {
        end = end.min(start + first);
    }
    if let Some(last) = last {
        start = start.max(end.saturating_sub(last));
    }
    let edges: Vec<Edge> = items[start..end]
        .iter()
        .map(|item| Edge {
            cursor: cursor(item),
            node: node(item),
        })
        .collect();
    let page_info = PageInfo {
        has_next_page: end < items.len(),
        has_previous_page: start > 0,
        start_cursor: edges.first().map(|e| e.cursor.clone()),
        end_cursor: edges.last().map(|e| e.cursor.clone()),
    };
    Ok(Resolved::Object {
        type_name,
        value: Arc::new(Conn {
            type_name,
            edges,
            page_info,
            total_count: items.len(),
        }),
    })
}

pub(crate) const MAX_PAGE: usize = 100;

fn page_size(args: &Args, name: &str) -> Result<Option<usize>, GqlError> {
    let Some(n) = args.get::<i64>(name)? else {
        return Ok(None);
    };
    let n = usize::try_from(n).ok().filter(|n| *n > 0).ok_or_else(|| {
        let requirement = if n < 0 {
            "cannot be less than zero"
        } else {
            "must be at least 1"
        };
        GqlError::new(
            ErrorType::InvalidPagination,
            format!("`{name}` on the connection {requirement}."),
        )
    })?;
    if n > MAX_PAGE {
        return Err(GqlError::new(
            ErrorType::ExcessivePagination,
            format!(
                "Requesting {n} records on the connection exceeds the `{name}` limit of {MAX_PAGE} records."
            ),
        ));
    }
    Ok(Some(n))
}

fn position<T>(
    items: &[T],
    cursor: &impl Fn(&T) -> String,
    wanted: &str,
    name: &str,
) -> Result<usize, GqlError> {
    items
        .iter()
        .position(|i| cursor(i) == wanted)
        .ok_or_else(|| {
            GqlError::new(
                ErrorType::Unprocessable,
                format!("`{name}` does not appear to be a valid cursor."),
            )
        })
}
