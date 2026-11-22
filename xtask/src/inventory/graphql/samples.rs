use tree_sitter::Node;

use super::super::Result;
use super::super::go::{Source, assignments, scope, walk};

const SHA: &str = "0123456789abcdef0123456789abcdef01234567";
const SAMPLE_PATHS: &[&str] = &[
    ".github/pull_request_template.md",
    "docs/PULL_REQUEST_TEMPLATE.md",
];

pub(super) fn render(
    source: &Source,
    query: Node<'_>,
    enclosing: &str,
) -> Result<Option<(&'static str, String)>> {
    let arg = source.text(query);
    let named = match (enclosing, arg) {
        (_, "implementIssueQuery") => ("implementIssue", blob_query(source, query, true)?),
        ("FilesAt", "query") => ("filesAt", blob_query(source, query, false)?),
        ("HoldsFiles", "query") => ("holdsFiles", holds_files(source, query)?),
        ("peelReleaseTags", "query") => ("peelTags", peel_tags(source, query)?),
        ("accessIn", "query") => ("access", access(source, query)?),
        (_, arg) if arg.starts_with("aliasedQuery(") && arg.ends_with("refAlias)") => {
            ("versionRef", aliased(source, "r", "refAlias", 1)?)
        }
        (_, arg) if arg.starts_with("aliasedQuery(") && arg.ends_with("blobAlias)") => {
            ("versionFiles", aliased(source, "e", "blobAlias", 2)?)
        }
        _ => return Ok(None),
    };
    Ok(Some(named))
}

fn blob_query(source: &Source, query: Node<'_>, issue: bool) -> Result<String> {
    let declaration = if issue { source.root() } else { scope(query) };
    let value = assigned(source, declaration, source.text(query), query.start_byte())?;
    let pieces: Vec<_> = literals(source, value).filter(|s| s != "HEAD").collect();
    let [opening, ending] = pieces.as_slice() else {
        return Err("unexpected blob query expression".into());
    };
    let function = function(source, "blobFields")?;
    let template = literal(source, function, "\n    t%d: object")?;
    let paths = if issue {
        literals(
            source,
            assigned(
                source,
                source.root(),
                "PullTemplatePaths",
                query.start_byte(),
            )?,
        )
        .collect()
    } else {
        SAMPLE_PATHS
            .iter()
            .map(|path| (*path).to_owned())
            .collect::<Vec<_>>()
    };
    let revision = if issue { "HEAD" } else { SHA };
    let mut rendered = opening.clone();
    for (index, name) in paths.iter().enumerate() {
        let path = serde_json::to_string(&format!("{revision}:{name}"))?;
        rendered.push_str(&substitute(&template, &[&index.to_string(), &path])?);
    }
    rendered.push_str(ending);
    Ok(rendered)
}

fn holds_files(source: &Source, query: Node<'_>) -> Result<String> {
    let body = scope(query);
    let declaration = literal(source, body, ", $e%d: String!")?;
    let selection = literal(source, body, " t%d: object")?;
    let template = literal(source, body, "query(")?;
    let (declared, selected) = pair(&declaration, &selection, "", "", 2)?;
    substitute(&template, &[&declared, &selected])
}

fn access(source: &Source, query: Node<'_>) -> Result<String> {
    let body = scope(query);
    let declaration = literal(source, body, "$n%d: String!")?;
    let selection = literal(source, body, "r%d: repository")?;
    let template = literal(source, body, "query(")?;
    let owner = literal(source, body, "$owner:")?;
    let login = literal(source, body, "$login:")?;
    let (declared, selected) = pair(&declaration, &selection, ", ", " ", 2)?;
    substitute(
        &template,
        &[&format!("{owner}, {login}, {declared}"), &selected],
    )
}

fn peel_tags(source: &Source, query: Node<'_>) -> Result<String> {
    let value = assigned(source, scope(query), source.text(query), query.start_byte())?;
    let parts: Vec<_> = literals(source, value).filter(|part| part != " ").collect();
    let [opening, ending] = parts.as_slice() else {
        return Err("unexpected tag query expression".into());
    };
    let selection = literal(source, scope(query), "t%d: object")?;
    Ok(format!(
        "{opening}{}{ending}",
        substitute(&selection, &["0", &serde_json::to_string(SHA)?])?
    ))
}

fn aliased(source: &Source, variable: &str, alias: &str, count: usize) -> Result<String> {
    let body = function(source, "aliasedQuery")?;
    let declaration = literal(source, body, ", $%s%d")?.replacen("%s", variable, 1);
    let field = assigned(source, source.root(), alias, usize::MAX)?;
    let selection = format!(
        " {}",
        source.string(field).ok_or("expected alias template")?
    );
    let (declared, selected) = pair(&declaration, &selection, "", "", count)?;
    let opening = literal(source, body, "query(")?;
    let middle = literal(source, body, ") { repository")?;
    let ending = literal(source, body, " } }")?;
    Ok(format!("{opening}{declared}{middle}{selected}{ending}"))
}

fn pair(
    declaration: &str,
    selection: &str,
    declaration_sep: &str,
    selection_sep: &str,
    count: usize,
) -> Result<(String, String)> {
    let mut declarations = Vec::new();
    let mut selections = Vec::new();
    for index in 0..count {
        let index = index.to_string();
        declarations.push(substitute(declaration, &[&index])?);
        selections.push(substitute(selection, &[&index, &index])?);
    }
    Ok((
        declarations.join(declaration_sep),
        selections.join(selection_sep),
    ))
}

fn substitute(format: &str, values: &[&str]) -> Result<String> {
    let mut rest = format;
    let mut out = String::new();
    for value in values {
        let (before, after) = rest.split_once('%').ok_or("missing query placeholder")?;
        let [b'd' | b'q' | b's', ..] = after.as_bytes() else {
            return Err("unsupported query placeholder".into());
        };
        out.push_str(before);
        out.push_str(value);
        rest = &after[1..];
    }
    if rest.contains('%') {
        return Err("extra query placeholder".into());
    }
    out.push_str(rest);
    Ok(out)
}

fn literals<'a>(source: &'a Source, body: Node<'a>) -> impl Iterator<Item = String> + 'a {
    walk(body).filter_map(|node| source.string(node))
}

fn literal(source: &Source, body: Node<'_>, prefix: &str) -> Result<String> {
    literals(source, body)
        .find(|value| value.starts_with(prefix))
        .ok_or_else(|| {
            format!(
                "{}: missing query text starting with {prefix:?}",
                source.location(body)
            )
            .into()
        })
}

fn assigned<'a>(source: &'a Source, body: Node<'a>, name: &str, before: usize) -> Result<Node<'a>> {
    assignments(source, body, name, before)
        .pop()
        .ok_or_else(|| format!("{}: missing query assignment {name}", source.location(body)).into())
}

fn function<'a>(source: &'a Source, name: &str) -> Result<Node<'a>> {
    super::children(source.root())
        .find(|node| {
            node.kind() == "function_declaration"
                && node
                    .child_by_field_name("name")
                    .is_some_and(|node| source.text(node) == name)
        })
        .ok_or_else(|| format!("{}: missing query builder {name}", source.path().display()).into())
}
