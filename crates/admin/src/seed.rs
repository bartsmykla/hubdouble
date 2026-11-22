use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use hubdouble_git::Git;
use hubdouble_model::{ApiError, Clock, Token};
use hubdouble_store::{Counters, Sequence, Tokens, TxMut};
use serde_json::Value;

use crate::{KindApply, SeedCtx};

pub(crate) struct Entry {
    pub position: usize,
    pub kind: &'static str,
    pub apply: KindApply,
    pub body: Value,
}

fn at(position: usize, kind: &str, mut e: ApiError) -> ApiError {
    e.message = format!("seed entry {position} ({kind}): {}", e.message);
    e
}

pub(crate) fn parse(path: &Path, text: &str) -> Result<Vec<Value>, ApiError> {
    let parsed = if path
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("json"))
    {
        serde_json::from_str::<Vec<Value>>(text).map_err(|e| e.to_string())
    } else {
        serde_saphyr::from_str::<Vec<Value>>(text).map_err(|e| e.to_string())
    };
    parsed.map_err(|e| ApiError::unprocessable(format!("seed {}: {e}", path.display())))
}

pub(crate) fn prepare(
    values: Vec<Value>,
    kinds: &[(&'static str, KindApply)],
    env: &dyn Fn(&str) -> Option<String>,
    base: Option<&Path>,
) -> Result<Vec<Entry>, ApiError> {
    let mut entries = Vec::with_capacity(values.len());
    for (i, v) in values.into_iter().enumerate() {
        let position = i + 1;
        let Value::Object(mut body) = v else {
            return Err(at(
                position,
                "?",
                ApiError::unprocessable("an entry is a mapping"),
            ));
        };
        let Some(Value::String(name)) = body.remove("kind") else {
            return Err(at(
                position,
                "?",
                ApiError::unprocessable("kind is missing"),
            ));
        };
        let Some(&(kind, apply)) = kinds.iter().find(|(k, _)| *k == name) else {
            return Err(at(position, &name, ApiError::unprocessable("no such kind")));
        };
        let mut body = Value::Object(body);
        crate::template::expand_env(&mut body, env).map_err(|e| at(position, kind, e))?;
        if let (Some(base), Some(Value::String(dir))) = (base, body.get_mut("from_dir")) {
            *dir = base.join(&*dir).to_string_lossy().into_owned();
        }
        entries.push(Entry {
            position,
            kind,
            apply,
            body,
        });
    }
    let order = |e: &Entry| {
        kinds
            .iter()
            .position(|(k, _)| *k == e.kind)
            .unwrap_or(usize::MAX)
    };
    entries.sort_by_key(order);
    Ok(forks_first(entries))
}

fn full_name(body: &Value) -> Option<String> {
    let owner = body.get("owner")?.as_str()?;
    let name = body.get("name")?.as_str()?;
    Some(format!("{owner}/{name}").to_ascii_lowercase())
}

fn forks_first(entries: Vec<Entry>) -> Vec<Entry> {
    let index: HashMap<(&str, String), usize> = entries
        .iter()
        .enumerate()
        .filter_map(|(i, e)| Some(((e.kind, full_name(&e.body)?), i)))
        .collect();
    let parent: Vec<Option<usize>> = entries
        .iter()
        .map(|e| {
            let fork_of = e.body.get("fork_of")?.as_str()?.to_ascii_lowercase();
            index.get(&(e.kind, fork_of)).copied()
        })
        .collect();
    let mut placed = vec![false; entries.len()];
    let mut order = Vec::with_capacity(entries.len());
    for i in 0..entries.len() {
        let mut chain = vec![i];
        while let Some(p) = parent[*chain.last().unwrap_or(&i)] {
            if placed[p] || chain.contains(&p) {
                break;
            }
            chain.push(p);
        }
        for j in chain.into_iter().rev() {
            if !placed[j] {
                placed[j] = true;
                order.push(j);
            }
        }
    }
    let mut slots: Vec<Option<Entry>> = entries.into_iter().map(Some).collect();
    order.into_iter().filter_map(|i| slots[i].take()).collect()
}

pub(crate) fn given_ids(value: &Value, out: &mut Vec<i64>) {
    match value {
        Value::Object(map) => {
            for (k, v) in map {
                match v {
                    Value::Number(n) if k == "id" => out.extend(n.as_i64()),
                    other => given_ids(other, out),
                }
            }
        }
        Value::Array(items) => items.iter().for_each(|v| given_ids(v, out)),
        _ => {}
    }
}

pub(crate) struct Env<'a> {
    pub git: &'a Git,
    pub clock: &'a Arc<dyn Clock>,
    pub lookup: &'a dyn Fn(&str) -> Option<String>,
}

pub(crate) fn apply(
    tx: &mut TxMut<'_>,
    entries: Vec<Entry>,
    env: &Env<'_>,
) -> Result<(), ApiError> {
    let mut ids = Vec::new();
    for e in &entries {
        given_ids(&e.body, &mut ids);
    }
    if let Some(max) = ids.into_iter().max() {
        tx.table_mut::<Counters>().bump(Sequence::Id, max);
    }
    for e in entries {
        let mut ctx = SeedCtx {
            tx: tx.reborrow(),
            git: env.git,
            clock: env.clock.as_ref(),
            env: env.lookup,
        };
        (e.apply)(&mut ctx, e.body).map_err(|err| at(e.position, e.kind, err))?;
    }
    tx.table_mut::<Counters>().mark_seeded();
    let unmarked: Vec<Token> = tx
        .table::<Tokens>()
        .iter()
        .filter(|t| !t.owner_seeded)
        .map(|t| Token {
            owner_seeded: true,
            ..Token::clone(t)
        })
        .collect();
    let mut tokens = tx.table_mut::<Tokens>();
    for t in unmarked {
        tokens.replace(t)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[expect(clippy::unnecessary_wraps, reason = "a kind returns a Result")]
    fn noop(_: &mut SeedCtx<'_>, _: Value) -> Result<(), ApiError> {
        Ok(())
    }

    const KINDS: [(&str, KindApply); 3] = [("user", noop), ("org", noop), ("repository", noop)];

    fn positions(entries: &[Entry]) -> Vec<usize> {
        entries.iter().map(|e| e.position).collect()
    }

    #[test]
    fn entries_apply_in_kind_order_then_file_order_with_fork_parents_first() {
        // Given
        let values = vec![
            json!({"kind": "repository", "owner": "o", "name": "fork", "fork_of": "O/Parent"}),
            json!({"kind": "org", "login": "o"}),
            json!({"kind": "repository", "owner": "o", "name": "other"}),
            json!({"kind": "user", "login": "a"}),
            json!({"kind": "repository", "owner": "o", "name": "parent"}),
            json!({"kind": "user", "login": "b"}),
        ];
        // When
        let entries = prepare(values, &KINDS, &|_| None, None).unwrap();
        // Then
        assert_eq!(positions(&entries), [4, 6, 2, 5, 1, 3]);
    }

    #[test]
    fn a_fork_chain_and_a_cycle_both_terminate() {
        let values = vec![
            json!({"kind": "repository", "owner": "o", "name": "c", "fork_of": "o/b"}),
            json!({"kind": "repository", "owner": "o", "name": "b", "fork_of": "o/a"}),
            json!({"kind": "repository", "owner": "o", "name": "a"}),
            json!({"kind": "repository", "owner": "o", "name": "x", "fork_of": "o/y"}),
            json!({"kind": "repository", "owner": "o", "name": "y", "fork_of": "o/x"}),
        ];
        let entries = prepare(values, &KINDS, &|_| None, None).unwrap();
        assert_eq!(positions(&entries), [3, 2, 1, 5, 4]);
    }

    #[test]
    fn an_unknown_kind_or_a_missing_variable_fails_with_its_position() {
        let unknown = prepare(
            vec![json!({"kind": "user"}), json!({"kind": "nope"})],
            &KINDS,
            &|_| None,
            None,
        );
        let e = unknown.err().unwrap();
        assert_eq!(e.status, 422);
        assert!(e.message.starts_with("seed entry 2 "), "{}", e.message);
        let missing = prepare(
            vec![json!({"kind": "user", "login": "{{env \"NOPE\"}}"})],
            &KINDS,
            &|_| None,
            None,
        );
        assert!(missing.err().unwrap().message.starts_with("seed entry 1 "));
    }

    #[test]
    fn given_ids_are_found_at_every_depth() {
        let mut ids = Vec::new();
        given_ids(
            &json!({"id": 5, "assets": [{"id": 9}], "app": 3, "x": {"id": "7"}}),
            &mut ids,
        );
        ids.sort_unstable();
        assert_eq!(ids, [5, 9]);
    }

    #[test]
    fn yaml_and_json_seeds_parse_to_the_same_entries() {
        let yaml = parse(Path::new("s.yaml"), "- kind: user\n  login: a\n  id: 7\n").unwrap();
        let json = parse(
            Path::new("s.json"),
            r#"[{"kind": "user", "login": "a", "id": 7}]"#,
        )
        .unwrap();
        assert_eq!(yaml, json);
        assert_eq!(
            parse(Path::new("s.yaml"), "kind: user").unwrap_err().status,
            422
        );
    }
}
