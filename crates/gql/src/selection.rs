use apollo_compiler::ast::{DirectiveList, Value as Ast};
use serde_json::{Map, Value};

pub(crate) fn included(directives: &DirectiveList, vars: &Map<String, Value>) -> bool {
    let flag = |name| match directives
        .get(name)?
        .specified_argument_by_name("if")?
        .as_ref()
    {
        Ast::Boolean(b) => Some(*b),
        Ast::Variable(n) => vars.get(n.as_str()).and_then(Value::as_bool),
        _ => None,
    };
    flag("skip") != Some(true) && flag("include") != Some(false)
}
