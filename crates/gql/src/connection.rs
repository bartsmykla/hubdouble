use std::sync::Arc;

use serde::Serialize;

use crate::contract::{Args, ErrorType, GqlError, Int, Resolved};

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

pub(crate) enum IncompletePage {
    MissingSize,
    InvalidCursor(CursorError),
}

pub(crate) struct PartialConnection {
    pub(crate) connection: Arc<Conn>,
    pub(crate) page: IncompletePage,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum CursorErrorFields {
    Nodes,
    Page,
    PageInfoFields,
}

pub(crate) struct CursorError {
    pub(crate) fields: CursorErrorFields,
    pub(crate) error: GqlError,
}

impl CursorError {
    fn new(type_name: &str, argument: &str, cursor: &str) -> Result<Self, GqlError> {
        use CursorErrorFields::{Nodes, Page, PageInfoFields};
        let (fields, kind) = match type_name {
            "RefConnection" | "PullRequestCommitConnection" => {
                (PageInfoFields, ErrorType::Validation)
            }
            "SearchResultItemConnection" => (PageInfoFields, ErrorType::InvalidCursorArguments),
            "PullRequestConnection" | "PullRequestReviewConnection" | "TeamConnection" => {
                (Page, ErrorType::InvalidCursorArguments)
            }
            "PullRequestReviewThreadConnection" | "PullRequestReviewCommentConnection" => {
                (Nodes, ErrorType::InvalidCursorArguments)
            }
            "RepositoryCollaboratorConnection" => {
                return Err(GqlError::new(
                    ErrorType::InvalidCursorArguments,
                    format!("`{cursor}` does not appear to be a valid cursor."),
                ));
            }
            _ => {
                return Err(GqlError::new(
                    ErrorType::Unprocessable,
                    format!("`{argument}` does not appear to be a valid cursor."),
                ));
            }
        };
        let message = if kind == ErrorType::Validation {
            format!("Invalid input: {cursor:?}")
        } else {
            format!("`{cursor}` does not appear to be a valid cursor.")
        };
        Ok(Self {
            fields,
            error: GqlError::new(kind, message),
        })
    }
}

/// Slices `items` by `first`, `last`, `after` and `before` into a [`Conn`] named `type_name`
/// (`PullRequestConnection`), an empty page included.
///
/// # Errors
///
/// GitHub's error for a negative page size, a size above 100, or both sizes.
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
    let empty = || Conn {
        type_name,
        edges: Vec::new(),
        page_info: PageInfo::default(),
        total_count: items.len(),
    };
    if first.is_none() && last.is_none() {
        return Ok(partial(empty(), IncompletePage::MissingSize));
    }
    let mut invalid = None;
    let mut start =
        boundary(items, &cursor, args.get("after")?, "after", &mut invalid).map_or(0, |i| i + 1);
    let mut end = boundary(items, &cursor, args.get("before")?, "before", &mut invalid)
        .unwrap_or(items.len());
    let error = if let Some((argument, token)) = invalid {
        let error = CursorError::new(type_name, argument, &token)?;
        if error.fields != CursorErrorFields::Nodes {
            return Ok(partial(empty(), IncompletePage::InvalidCursor(error)));
        }
        Some(error)
    } else {
        None
    };
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
    let connection = Conn {
        type_name,
        edges,
        page_info,
        total_count: items.len(),
    };
    Ok(match error {
        Some(error) => partial(connection, IncompletePage::InvalidCursor(error)),
        None => Resolved::Object {
            type_name,
            value: Arc::new(connection),
        },
    })
}

pub(crate) const MAX_PAGE: usize = 100;

fn page_size(args: &Args, name: &str) -> Result<Option<usize>, GqlError> {
    let negative = || {
        GqlError::new(
            ErrorType::InvalidPagination,
            format!("`{name}` on the connection cannot be less than zero."),
        )
    };
    let n = match args.int(name)? {
        None => return Ok(None),
        Some(Int::Fits(n)) if n < 0 => return Err(negative()),
        Some(Int::Past(digits)) if digits.starts_with('-') => return Err(negative()),
        Some(n) => n,
    };
    let size = match n {
        Int::Fits(size) => usize::try_from(size).ok().filter(|size| *size <= MAX_PAGE),
        Int::Past(_) => None,
    };
    size.map(Some).ok_or_else(|| {
        GqlError::new(
            ErrorType::ExcessivePagination,
            format!(
                "Requesting {n} records on the connection exceeds the `{name}` limit of {MAX_PAGE} records."
            ),
        )
    })
}

fn boundary<T>(
    items: &[T],
    cursor: &impl Fn(&T) -> String,
    wanted: Option<String>,
    argument: &'static str,
    invalid: &mut Option<(&'static str, String)>,
) -> Option<usize> {
    let wanted = wanted?;
    let index = items.iter().position(|i| cursor(i) == wanted);
    if index.is_none() {
        invalid.get_or_insert((argument, wanted));
    }
    index
}

fn partial(connection: Conn, page: IncompletePage) -> Resolved {
    Resolved::Object {
        type_name: connection.type_name,
        value: Arc::new(PartialConnection {
            connection: Arc::new(connection),
            page,
        }),
    }
}
