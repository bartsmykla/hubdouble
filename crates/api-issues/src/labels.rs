use std::sync::Arc;

use hubdouble_model::{ApiError, Label, Repo, RepoRole, node_id};
use hubdouble_rest::{Ctx, Reply};
use hubdouble_store::{Labels, TxMut};
use serde::Deserialize;
use serde_json::{Map, Value};

use crate::shared::{
    DEFAULT_LABEL_COLOR, changed_from, created, emit, full_name, invalid, label_view, payload_urls,
    to_value, who,
};

const MAX_DESCRIPTION: usize = 100;

fn valid_color(c: &str) -> bool {
    c.len() == 6 && c.bytes().all(|b| b.is_ascii_hexdigit())
}

fn check(
    name: Option<&str>,
    color: Option<&str>,
    description: Option<&str>,
) -> Result<(), ApiError> {
    if name.is_some_and(|n| n.trim().is_empty()) {
        return Err(invalid("Label", "name", "missing_field"));
    }
    if color.is_some_and(|c| !valid_color(c)) {
        return Err(invalid("Label", "color", "invalid"));
    }
    if description.is_some_and(|d| d.chars().count() > MAX_DESCRIPTION) {
        return Err(invalid("Label", "description", "invalid"));
    }
    Ok(())
}

fn label_event(
    tx: &mut TxMut<'_>,
    repo: &Repo,
    label: &Label,
    action: &'static str,
    sender: i64,
    changes: Option<Value>,
) -> Result<(), ApiError> {
    let urls = payload_urls(tx);
    let full = full_name(tx, repo);
    let mut p = Map::new();
    p.insert(
        "label".to_owned(),
        to_value(&label_view(&urls, &full, label))?,
    );
    if let Some(c) = changes {
        p.insert("changes".to_owned(), c);
    }
    emit(tx, "label", action, repo, sender, p);
    Ok(())
}

fn insert(
    tx: &mut TxMut<'_>,
    repo: &Repo,
    sender: i64,
    name: String,
    color: String,
    description: Option<String>,
) -> Result<Arc<Label>, ApiError> {
    if tx.table::<Labels>().by_name(repo.id, &name).is_some() {
        return Err(invalid("Label", "name", "already_exists"));
    }
    let id = tx.next_id();
    let label = tx.table_mut::<Labels>().insert(Label {
        id,
        node_id: node_id::entity(node_id::LABEL, id),
        repo_id: repo.id,
        name,
        color,
        description,
        default: false,
    })?;
    label_event(tx, repo, &label, "created", sender, None)?;
    Ok(label)
}

pub(crate) fn ensure(
    tx: &mut TxMut<'_>,
    repo: &Repo,
    sender: i64,
    names: &[String],
) -> Result<Vec<Arc<Label>>, ApiError> {
    let mut out: Vec<Arc<Label>> = Vec::with_capacity(names.len());
    for name in names {
        check(Some(name), None, None)?;
        let label = match tx.table::<Labels>().by_name(repo.id, name).cloned() {
            Some(l) => l,
            None => insert(
                tx,
                repo,
                sender,
                name.clone(),
                DEFAULT_LABEL_COLOR.to_owned(),
                None,
            )?,
        };
        if !out.iter().any(|l| l.id == label.id) {
            out.push(label);
        }
    }
    Ok(out)
}

pub(crate) fn list(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let repo = ctx.repo()?.clone();
    let tx = ctx.tx();
    let mut labels: Vec<&Arc<Label>> = tx.table::<Labels>().in_repo(repo.id).collect();
    labels.sort_by_cached_key(|l| l.name.to_lowercase());
    let urls = ctx.urls();
    let full = full_name(tx, &repo);
    ctx.page(&labels, |l| label_view(urls, &full, l))
}

#[derive(Deserialize)]
struct NewLabel {
    name: Option<String>,
    color: Option<String>,
    description: Option<String>,
}

pub(crate) fn create(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let input: NewLabel = ctx.body()?;
    let repo = ctx.repo()?.clone();
    let name = input
        .name
        .ok_or_else(|| invalid("Label", "name", "missing_field"))?;
    check(
        Some(&name),
        input.color.as_deref(),
        input.description.as_deref(),
    )?;
    let who = who(ctx, &repo)?;
    if !who.can(RepoRole::Write) {
        return Err(ApiError::forbidden("Must have push access to repository"));
    }
    let urls = ctx.urls().clone();
    let tx = ctx.tx_mut()?;
    let color = input
        .color
        .unwrap_or_else(|| DEFAULT_LABEL_COLOR.to_owned());
    let label = insert(tx, &repo, who.account.id, name, color, input.description)?;
    let view = label_view(&urls, &full_name(tx, &repo), &label);
    created(&view, &view.url)
}

pub(crate) fn update(ctx: &mut Ctx<'_>) -> Result<Reply, ApiError> {
    let input: Map<String, Value> = ctx.body()?;
    let text = |key: &str| -> Result<Option<String>, ApiError> {
        match input.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(s)) => Ok(Some(s.clone())),
            Some(_) => Err(invalid("Label", key, "invalid")),
        }
    };
    let new_name = text("new_name")?;
    let color = text("color")?;
    let description = text("description")?;
    check(
        new_name.as_deref(),
        color.as_deref(),
        description.as_deref(),
    )?;
    let repo = ctx.repo()?.clone();
    let name = ctx.param("name")?.to_owned();
    let who = who(ctx, &repo)?;
    if !who.can(RepoRole::Write) {
        return Err(ApiError::forbidden("Must have push access to repository"));
    }
    let urls = ctx.urls().clone();
    let tx = ctx.tx_mut()?;
    let old = tx
        .table::<Labels>()
        .by_name(repo.id, &name)
        .cloned()
        .ok_or_else(ApiError::not_found)?;
    if let Some(n) = &new_name
        && tx
            .table::<Labels>()
            .by_name(repo.id, n)
            .is_some_and(|l| l.id != old.id)
    {
        return Err(invalid("Label", "name", "already_exists"));
    }
    let mut changes = Map::new();
    let mut label = Label::clone(&old);
    if let Some(n) = new_name.filter(|n| *n != old.name) {
        changes.extend(changed_from("name", Value::from(old.name.clone())));
        label.name = n;
    }
    if let Some(c) = color.filter(|c| *c != old.color) {
        changes.extend(changed_from("color", Value::from(old.color.clone())));
        label.color = c;
    }
    if input.contains_key("description") && description != old.description {
        changes.extend(changed_from(
            "description",
            old.description.clone().map_or(Value::Null, Value::from),
        ));
        label.description = description;
    }
    let label = Arc::new(label);
    if !changes.is_empty() {
        tx.table_mut::<Labels>().replace(label.clone())?;
        label_event(
            tx,
            &repo,
            &label,
            "edited",
            who.account.id,
            Some(Value::Object(changes)),
        )?;
    }
    Reply::json(&label_view(&urls, &full_name(tx, &repo), &label))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_color_is_six_hex_digits_without_a_hash() {
        assert!(valid_color("ededed"));
        assert!(valid_color("B60205"));
        assert!(!valid_color("#ededed"));
        assert!(!valid_color("fff"));
        assert!(!valid_color("gggggg"));
    }
}
