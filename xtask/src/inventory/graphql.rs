mod samples;

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;

use tree_sitter::Node;

use super::Result;
use super::go::{Source, arguments, assignments, children, scope, walk};

pub(super) fn scan(source: &Source, documents: &mut BTreeMap<String, String>) -> Result<()> {
    if source
        .path()
        .file_name()
        .is_some_and(|name| name.to_string_lossy().ends_with("_test.go"))
    {
        return Ok(());
    }
    for call in walk(source.root()).filter(|node| node.kind() == "call_expression") {
        let Some(function) = call.child_by_field_name("function") else {
            continue;
        };
        let args = arguments(call);
        let Some(query) = query_argument(source, function, &args)? else {
            continue;
        };
        let enclosing = scope(call);
        let name = enclosing
            .child_by_field_name("name")
            .map_or("", |node| source.text(node));
        if matches!(
            name,
            "GraphQL" | "ask" | "askIn" | "walkThreads" | "walkThreadsFrom"
        ) && source.text(query) == "query"
        {
            continue;
        }
        let (name, document) = document(source, query, name)?;
        if let Some(previous) = documents.insert(name.clone(), document.clone())
            && previous != document
        {
            return Err(format!(
                "{}: GraphQL document name {name} describes multiple queries",
                source.location(call)
            )
            .into());
        }
    }
    Ok(())
}

fn query_argument<'a>(
    source: &'a Source,
    function: Node<'a>,
    args: &[Node<'a>],
) -> Result<Option<Node<'a>>> {
    let name = source.text(function);
    let index = if matches!(
        name,
        "GraphQL" | "gitwrite.GraphQL" | "s.askIn" | "walkThreads" | "walkThreadsFrom"
    ) {
        2
    } else if name == "s.ask" {
        1
    } else if name.ends_with(".NewRequest")
        && args.get(2).is_some_and(|path| {
            super::rest::values(source, *path, 0).iter().any(|path| {
                matches!(
                    path.trim_start_matches('/'),
                    "graphql" | "api/graphql" | "api/v3/graphql"
                )
            })
        })
    {
        let body = args.get(3).copied().ok_or("GraphQL request has no body")?;
        return body_query(source, body).map(Some).ok_or_else(|| {
            format!("{}: unresolved GraphQL request body", source.location(body)).into()
        });
    } else {
        return Ok(None);
    };
    args.get(index).copied().map(Some).ok_or_else(|| {
        format!(
            "{}: missing GraphQL query argument",
            source.location(function)
        )
        .into()
    })
}

fn body_query<'a>(source: &'a Source, body: Node<'a>) -> Option<Node<'a>> {
    let body = if body.kind() == "identifier" {
        *assignments(source, scope(body), source.text(body), body.start_byte()).last()?
    } else {
        body
    };
    let query = walk(body).find(|node| {
        node.kind() == "keyed_element"
            && node
                .child_by_field_name("key")
                .is_some_and(|key| source.text(key) == "\"query\"")
    })?;
    let value = query.child_by_field_name("value")?;
    children(value).next().or(Some(value))
}

fn document(source: &Source, arg: Node<'_>, enclosing: &str) -> Result<(String, String)> {
    let text = source.text(arg);
    if let Some((name, doc)) = samples::render(source, arg, enclosing)? {
        return Ok((name.to_owned(), newline(doc)));
    }
    let (name, value) = if arg.kind() == "identifier" {
        let local = assignments(source, scope(arg), text, arg.start_byte())
            .last()
            .copied();
        let global = || {
            children(source.root())
                .filter(|node| matches!(node.kind(), "const_declaration" | "var_declaration"))
                .flat_map(|node| assignments(source, node, text, usize::MAX))
                .next()
        };
        let value = local
            .or_else(global)
            .ok_or_else(|| format!("{}: unresolved GraphQL query {text}", source.location(arg)))?;
        (if local.is_some() { enclosing } else { text }, value)
    } else {
        (enclosing, arg)
    };
    let text = static_text(source, value, 0).ok_or_else(|| {
        format!(
            "{}: GraphQL query from {} needs a rendered sample",
            source.location(value),
            value.kind()
        )
    })?;
    Ok((document_name(name).into(), newline(text)))
}

fn static_text(source: &Source, value: Node<'_>, depth: u8) -> Option<String> {
    if depth == 16 {
        return None;
    }
    match value.kind() {
        "binary_expression" if source.text(value.child_by_field_name("operator")?) == "+" => {
            let left = static_text(source, value.child_by_field_name("left")?, depth + 1)?;
            let right = static_text(source, value.child_by_field_name("right")?, depth + 1)?;
            Some(left + &right)
        }
        "identifier" => {
            let constant = children(source.root())
                .filter(|node| node.kind() == "const_declaration")
                .flat_map(|node| assignments(source, node, source.text(value), usize::MAX))
                .next()?;
            static_text(source, constant, depth + 1)
        }
        _ => source.string(value),
    }
}

fn document_name(name: &str) -> &str {
    match name {
        "openQuery" => "proposalOpen",
        "listQuery" => "proposals",
        _ => name
            .strip_suffix("Query")
            .or_else(|| name.strip_suffix("Mutation"))
            .unwrap_or(name),
    }
}

fn newline(mut document: String) -> String {
    document.truncate(document.trim_end().len());
    document.push('\n');
    document
}

pub(super) fn fields(inventory: &Path, documents: &BTreeMap<String, String>) -> Result<String> {
    let schema = hubdouble_gql::Schema::load().map_err(|error| format!("{error:?}"))?;
    let assignments = std::fs::read_to_string(inventory.join("graphql-fields.tsv"))?;
    let owners: BTreeMap<_, _> = assignments
        .lines()
        .skip(1)
        .filter_map(|line| {
            let mut cols = line.split('\t');
            Some((cols.next()?, cols.next()?))
        })
        .filter(|(_, owner)| !owner.is_empty())
        .collect();
    let mut rows: BTreeMap<String, Vec<&str>> = BTreeMap::new();
    for (name, document) in documents {
        let fields = schema
            .selected_fields(document)
            .map_err(|error| format!("{name}: {error:?}"))?;
        for (ty, field) in fields {
            rows.entry(format!("{ty}.{field}")).or_default().push(name);
        }
    }
    let missing: Vec<_> = rows
        .keys()
        .filter(|field| !owners.contains_key(field.as_str()))
        .collect();
    if !missing.is_empty() {
        return Err(format!("assign an owner in graphql-fields.tsv: {missing:?}").into());
    }
    let mut output = "field\towner\tdocuments\n".to_owned();
    for (field, documents) in rows {
        writeln!(
            output,
            "{field}\t{}\t{}",
            owners[field.as_str()],
            documents.join(",")
        )?;
    }
    Ok(output)
}
