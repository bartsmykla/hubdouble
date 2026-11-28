use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use hubdouble_model::{ApiError, FieldError};

use crate::store::{Table, TableMut};

/// A row type of a [`Rows`] table: its ID and the index keys it is found by. A track's side
/// table can be `Rows<TheTracksRow>` with its own `Entity` impl.
pub trait Entity: Send + Sync + 'static {
    fn id(&self) -> i64;
    fn node_id(&self) -> Option<&str> {
        None
    }
    /// A second ID the row is found by, such as a pull request's own ID next to its issue ID.
    fn alt_id(&self) -> Option<i64> {
        None
    }
    fn alt_node_id(&self) -> Option<&str> {
        None
    }
    fn repo_id(&self) -> Option<i64> {
        None
    }
    /// The number within [`Entity::repo_id`], unique per repository.
    fn number(&self) -> Option<i64> {
        None
    }
    /// The owning row, such as a comment's issue.
    fn parent_id(&self) -> Option<i64> {
        None
    }
    /// Unique lookup keys, already normalized (lowercase where GitHub ignores case).
    fn keys(&self) -> Vec<String> {
        Vec::new()
    }
    /// Whether [`Table::restore`] keeps the current row over the seeded state.
    fn keep_on_reset(&self) -> bool {
        false
    }
}

/// An index key that another row already holds.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{what} is already taken")]
pub struct Taken {
    pub what: &'static str,
}

impl From<Taken> for ApiError {
    fn from(t: Taken) -> ApiError {
        ApiError::unprocessable("Validation Failed").with_error(FieldError {
            resource: "hubdouble".to_owned(),
            field: t.what.to_owned(),
            code: "already_exists".to_owned(),
            message: None,
            value: None,
        })
    }
}

/// A table of `Arc` rows indexed by ID, node ID, repository, number, parent and keys.
/// Iteration is in ID order.
#[expect(
    clippy::struct_field_names,
    reason = "each field is the index by one attribute"
)]
pub struct Rows<E> {
    by_id: BTreeMap<i64, Arc<E>>,
    by_node: HashMap<Box<str>, i64>,
    by_alt: HashMap<i64, i64>,
    by_repo: HashMap<i64, BTreeSet<i64>>,
    by_parent: HashMap<i64, BTreeSet<i64>>,
    by_number: HashMap<(i64, i64), i64>,
    by_key: HashMap<String, i64>,
}

impl<E> Default for Rows<E> {
    fn default() -> Rows<E> {
        Rows {
            by_id: BTreeMap::new(),
            by_node: HashMap::new(),
            by_alt: HashMap::new(),
            by_repo: HashMap::new(),
            by_parent: HashMap::new(),
            by_number: HashMap::new(),
            by_key: HashMap::new(),
        }
    }
}

impl<E> Clone for Rows<E> {
    fn clone(&self) -> Rows<E> {
        Rows {
            by_id: self.by_id.clone(),
            by_node: self.by_node.clone(),
            by_alt: self.by_alt.clone(),
            by_repo: self.by_repo.clone(),
            by_parent: self.by_parent.clone(),
            by_number: self.by_number.clone(),
            by_key: self.by_key.clone(),
        }
    }
}

impl<E: Entity> Table for Rows<E> {
    fn restore(&mut self, seeded: &Rows<E>) {
        let kept: Vec<Arc<E>> = self
            .by_id
            .values()
            .filter(|e| e.keep_on_reset())
            .cloned()
            .collect();
        *self = seeded.clone();
        for e in kept {
            self.raw_replace(e);
        }
    }
}

impl<E: Entity> Rows<E> {
    #[must_use]
    pub fn get(&self, id: i64) -> Option<&Arc<E>> {
        self.by_id.get(&id)
    }

    #[must_use]
    pub fn by_node_id(&self, node_id: &str) -> Option<&Arc<E>> {
        self.by_node.get(node_id).and_then(|id| self.get(*id))
    }

    #[must_use]
    pub fn by_alt_id(&self, alt_id: i64) -> Option<&Arc<E>> {
        self.by_alt.get(&alt_id).and_then(|id| self.get(*id))
    }

    #[must_use]
    pub fn by_number(&self, repo_id: i64, number: i64) -> Option<&Arc<E>> {
        self.by_number
            .get(&(repo_id, number))
            .and_then(|id| self.get(*id))
    }

    #[must_use]
    pub fn by_key(&self, key: &str) -> Option<&Arc<E>> {
        self.by_key.get(key).and_then(|id| self.get(*id))
    }

    pub fn iter(&self) -> impl DoubleEndedIterator<Item = &Arc<E>> {
        self.by_id.values()
    }

    /// The rows of one repository, in ID order.
    pub fn in_repo(&self, repo_id: i64) -> impl DoubleEndedIterator<Item = &Arc<E>> {
        self.ids_in(&self.by_repo, repo_id)
    }

    /// The rows whose [`Entity::parent_id`] is `parent_id`, in ID order.
    pub fn children(&self, parent_id: i64) -> impl DoubleEndedIterator<Item = &Arc<E>> {
        self.ids_in(&self.by_parent, parent_id)
    }

    fn ids_in<'a>(
        &'a self,
        index: &'a HashMap<i64, BTreeSet<i64>>,
        key: i64,
    ) -> impl DoubleEndedIterator<Item = &'a Arc<E>> {
        index
            .get(&key)
            .into_iter()
            .flatten()
            .filter_map(|id| self.by_id.get(id))
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.by_id.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }

    fn check_free(&self, e: &E, replacing: bool) -> Result<(), Taken> {
        let owner = e.id();
        let free = |held: Option<&i64>| held.is_none_or(|id| *id == owner);
        if !replacing && self.by_id.contains_key(&owner) {
            return Err(Taken { what: "id" });
        }
        for node in [e.node_id(), e.alt_node_id()].into_iter().flatten() {
            if !free(self.by_node.get(node)) {
                return Err(Taken { what: "node_id" });
            }
        }
        if let Some(alt) = e.alt_id()
            && !free(self.by_alt.get(&alt))
        {
            return Err(Taken { what: "id" });
        }
        if let (Some(repo), Some(n)) = (e.repo_id(), e.number())
            && !free(self.by_number.get(&(repo, n)))
        {
            return Err(Taken { what: "number" });
        }
        if e.keys().iter().any(|k| !free(self.by_key.get(k))) {
            return Err(Taken { what: "name" });
        }
        Ok(())
    }

    fn raw_replace(&mut self, e: Arc<E>) -> Option<Arc<E>> {
        let old = self.raw_remove(e.id());
        let id = e.id();
        for node in [e.node_id(), e.alt_node_id()].into_iter().flatten() {
            self.by_node.insert(node.into(), id);
        }
        if let Some(alt) = e.alt_id() {
            self.by_alt.insert(alt, id);
        }
        if let Some(repo) = e.repo_id() {
            self.by_repo.entry(repo).or_default().insert(id);
            if let Some(n) = e.number() {
                self.by_number.insert((repo, n), id);
            }
        }
        if let Some(parent) = e.parent_id() {
            self.by_parent.entry(parent).or_default().insert(id);
        }
        for k in e.keys() {
            self.by_key.insert(k, id);
        }
        self.by_id.insert(id, e);
        old
    }

    fn raw_remove(&mut self, id: i64) -> Option<Arc<E>> {
        let e = self.by_id.remove(&id)?;
        for node in [e.node_id(), e.alt_node_id()].into_iter().flatten() {
            self.by_node.remove(node);
        }
        if let Some(alt) = e.alt_id() {
            self.by_alt.remove(&alt);
        }
        if let Some(repo) = e.repo_id() {
            remove_from(&mut self.by_repo, repo, id);
            if let Some(n) = e.number() {
                self.by_number.remove(&(repo, n));
            }
        }
        if let Some(parent) = e.parent_id() {
            remove_from(&mut self.by_parent, parent, id);
        }
        for k in e.keys() {
            self.by_key.remove(&k);
        }
        Some(e)
    }
}

fn remove_from(index: &mut HashMap<i64, BTreeSet<i64>>, key: i64, id: i64) {
    if let Some(set) = index.get_mut(&key) {
        set.remove(&id);
        if set.is_empty() {
            index.remove(&key);
        }
    }
}

impl<E: Entity> TableMut<'_, Rows<E>> {
    /// Adds a new row.
    ///
    /// # Errors
    ///
    /// [`Taken`] when its ID or one of its index keys is held by another row.
    pub fn insert(&mut self, e: impl Into<Arc<E>>) -> Result<Arc<E>, Taken> {
        let e = e.into();
        self.table.check_free(&e, false)?;
        self.table.raw_replace(e.clone());
        let id = e.id();
        self.record(move |t| {
            t.raw_remove(id);
        });
        Ok(e)
    }

    /// Adds a row or replaces the row with its ID; returns the replaced row.
    ///
    /// # Errors
    ///
    /// [`Taken`] when one of its index keys is held by another row.
    pub fn replace(&mut self, e: impl Into<Arc<E>>) -> Result<Option<Arc<E>>, Taken> {
        let e = e.into();
        self.table.check_free(&e, true)?;
        let id = e.id();
        let old = self.table.raw_replace(e);
        let restored = old.clone();
        self.record(move |t| {
            t.raw_remove(id);
            if let Some(old) = restored {
                t.raw_replace(old);
            }
        });
        Ok(old)
    }

    pub fn remove(&mut self, id: i64) -> Option<Arc<E>> {
        let old = self.table.raw_remove(id)?;
        let restored = old.clone();
        self.record(move |t| {
            t.raw_replace(restored);
        });
        Some(old)
    }

    /// Removes every row for which `doomed` holds.
    pub fn remove_where(&mut self, mut doomed: impl FnMut(&E) -> bool) {
        let ids: Vec<i64> = self
            .table
            .iter()
            .filter(|e| doomed(e))
            .map(|e| e.id())
            .collect();
        self.remove_all(ids);
    }

    /// Removes every row of one repository.
    pub fn remove_in_repo(&mut self, repo_id: i64) {
        let ids: Vec<i64> = self.table.in_repo(repo_id).map(|e| e.id()).collect();
        self.remove_all(ids);
    }

    /// Removes every row whose [`Entity::parent_id`] is `parent_id`.
    pub fn remove_children(&mut self, parent_id: i64) {
        let ids: Vec<i64> = self.table.children(parent_id).map(|e| e.id()).collect();
        self.remove_all(ids);
    }

    /// Removes the rows with these IDs; an unknown ID is skipped.
    pub fn remove_all(&mut self, ids: impl IntoIterator<Item = i64>) {
        for id in ids {
            self.remove(id);
        }
    }
}
