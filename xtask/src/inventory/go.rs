use std::fs;
use std::path::{Path, PathBuf};

use tree_sitter::{Node, Parser, Tree};

use super::Result;

pub(super) fn parser() -> Result<Parser> {
    let mut parser = Parser::new();
    parser.set_language(&tree_sitter_go::LANGUAGE.into())?;
    Ok(parser)
}

pub(super) fn files(root: &Path) -> Result<Vec<PathBuf>> {
    let mut pending = vec![root.to_owned()];
    let mut paths = Vec::new();
    while let Some(dir) = pending.pop() {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            if kind.is_dir() && entry.file_name() != ".git" {
                pending.push(entry.path());
            } else if kind.is_file() && entry.path().extension().is_some_and(|ext| ext == "go") {
                paths.push(entry.path());
            }
        }
    }
    paths.sort_unstable();
    Ok(paths)
}

pub(super) struct Source {
    path: PathBuf,
    code: String,
    tree: Tree,
}

impl Source {
    pub(super) fn read(parser: &mut Parser, path: PathBuf) -> Result<Self> {
        let code = fs::read_to_string(&path)?;
        let mut tree = parser.parse(&code, None).ok_or("Go parser stopped")?;
        if tree.root_node().has_error() {
            let mut compatible = code.clone();
            for (at, word) in code.match_indices("new") {
                let end = at + word.len();
                let prefix = code[..at].chars().next_back();
                if prefix.is_none_or(|c| !c.is_alphanumeric() && c != '_')
                    && code[end..]
                        .trim_start()
                        .strip_prefix('(')
                        .is_some_and(value_argument)
                {
                    compatible.replace_range(at..end, "NEW");
                }
            }
            tree = parser.parse(&compatible, None).ok_or("Go parser stopped")?;
        }
        if tree.root_node().has_error() {
            let node = walk(tree.root_node())
                .find(|node| node.is_error() || node.is_missing())
                .unwrap_or_else(|| tree.root_node());
            return Err(format!("{}:{}: invalid Go syntax", path.display(), line(node)).into());
        }
        Ok(Self { path, code, tree })
    }

    pub(super) fn root(&self) -> Node<'_> {
        self.tree.root_node()
    }

    pub(super) fn path(&self) -> &Path {
        &self.path
    }

    pub(super) fn text(&self, node: Node<'_>) -> &str {
        &self.code[node.byte_range()]
    }

    pub(super) fn location(&self, node: Node<'_>) -> String {
        format!("{}:{}", self.path.display(), line(node))
    }

    pub(super) fn string(&self, node: Node<'_>) -> Option<String> {
        let s = self.text(node);
        match node.kind() {
            "raw_string_literal" => Some(s.strip_prefix('`')?.strip_suffix('`')?.replace('\r', "")),
            "interpreted_string_literal" => serde_json::from_str(s).ok(),
            _ => None,
        }
    }

    pub(super) fn selector<'a>(&self, node: Node<'a>) -> Option<(Node<'a>, &'_ str)> {
        (node.kind() == "selector_expression").then_some(())?;
        Some((
            node.child_by_field_name("operand")?,
            self.text(node.child_by_field_name("field")?),
        ))
    }
}

fn value_argument(arg: &str) -> bool {
    let arg = arg.trim_start();
    ![
        "[]",
        "map[",
        "chan ",
        "<-chan",
        "func(",
        "struct{",
        "interface{",
    ]
    .iter()
    .any(|prefix| arg.starts_with(prefix))
}

fn line(node: Node<'_>) -> usize {
    node.start_position().row + 1
}

pub(super) fn children(node: Node<'_>) -> impl Iterator<Item = Node<'_>> {
    (0..node.named_child_count()).filter_map(move |i| node.named_child(u32::try_from(i).ok()?))
}

pub(super) fn walk(node: Node<'_>) -> impl Iterator<Item = Node<'_>> {
    let mut cursor = node.walk();
    let mut done = false;
    std::iter::from_fn(move || {
        if done {
            return None;
        }
        let current = cursor.node();
        if !cursor.goto_first_child() {
            while !cursor.goto_next_sibling() {
                if !cursor.goto_parent() {
                    done = true;
                    break;
                }
            }
        }
        Some(current)
    })
}

pub(super) fn arguments(call: Node<'_>) -> Vec<Node<'_>> {
    call.child_by_field_name("arguments")
        .into_iter()
        .flat_map(children)
        .filter(|node| node.kind() != "comment")
        .collect()
}

pub(super) fn scope(mut node: Node<'_>) -> Node<'_> {
    while let Some(parent) = node.parent() {
        if matches!(node.kind(), "method_declaration" | "function_declaration") {
            break;
        }
        node = parent;
    }
    node
}

pub(super) fn assignments<'a>(
    source: &'a Source,
    scope: Node<'a>,
    name: &str,
    before: usize,
) -> Vec<Node<'a>> {
    walk(scope)
        .filter(|node| node.start_byte() < before)
        .filter_map(|node| {
            let (left, right) = binding_lists(node)?;
            let index = children(left).position(|item| source.text(item) == name)?;
            children(right).nth(index)
        })
        .collect()
}

pub(super) fn binding_lists(node: Node<'_>) -> Option<(Node<'_>, Node<'_>)> {
    match node.kind() {
        "assignment_statement" | "short_var_declaration" => Some((
            node.child_by_field_name("left")?,
            node.child_by_field_name("right")?,
        )),
        "const_spec" | "var_spec" => Some((node, node.child_by_field_name("value")?)),
        _ => None,
    }
}
