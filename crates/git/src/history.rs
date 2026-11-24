use std::cmp::Reverse;
use std::collections::hash_map::Entry;
use std::collections::{BTreeSet, BinaryHeap, HashMap, HashSet};
use std::sync::Arc;

use hubdouble_store::Tx;

use crate::contract::{Commit, Git, GitError, ObjectData, ObjectId, RefChange};
use crate::diff::tree_changes;
use crate::odb::Odb;

const FROM_TIP: u8 = 1;
const FROM_OTHERS: u8 = 2;

struct Painter<'o> {
    odb: &'o Odb,
    seen: HashMap<ObjectId, (u8, u64, Arc<Commit>)>,
    queue: BinaryHeap<(u64, ObjectId)>,
    only_tip: BTreeSet<(u64, ObjectId)>,
}

impl Painter<'_> {
    fn paint(&mut self, id: ObjectId, flag: u8) -> Result<(), GitError> {
        let entry = match self.seen.entry(id) {
            Entry::Occupied(e) => e.into_mut(),
            Entry::Vacant(v) => v.insert((0, self.odb.generation(id), self.odb.commit(id)?)),
        };
        if entry.0 & flag == flag {
            return Ok(());
        }
        let generation = entry.1;
        let before = entry.0;
        entry.0 |= flag;
        match (before == FROM_TIP, entry.0 == FROM_TIP) {
            (false, true) => {
                self.only_tip.insert((generation, id));
            }
            (true, false) => {
                self.only_tip.remove(&(generation, id));
            }
            _ => {}
        }
        self.queue.push((generation, id));
        Ok(())
    }

    fn done(&self) -> bool {
        match (self.only_tip.first(), self.queue.peek()) {
            (None, _) | (_, None) => true,
            (Some((oldest, _)), Some((newest, _))) => newest < oldest,
        }
    }
}

impl Git {
    /// The commits reachable from `tip` and from none of `others`, children before their
    /// parents, as `git rev-list <tip> --not <others>`. The walk goes back by generation (one
    /// more than the highest parent's, kept with each commit) and stops once every commit left
    /// to read has a lower generation than every commit only `tip` reaches, so it reads the
    /// history only down to where `tip` meets `others`, whatever the commit times say. Tags
    /// are peeled, and a ref that does not peel to a commit, such as a tag of a tree, reaches
    /// no commit.
    ///
    /// # Errors
    ///
    /// [`GitError::NotFound`] when a commit is missing.
    pub fn exclusive_commits(
        &self,
        tx: &Tx,
        repo: i64,
        tip: ObjectId,
        others: &[ObjectId],
    ) -> Result<Vec<(ObjectId, Arc<Commit>)>, GitError> {
        let odb = self.odb(tx, repo);
        let mut painter = Painter {
            odb: &odb,
            seen: HashMap::new(),
            queue: BinaryHeap::new(),
            only_tip: BTreeSet::new(),
        };
        for &other in others {
            if let Some(other) = commit_of(&odb, other)? {
                painter.paint(other, FROM_OTHERS)?;
            }
        }
        let Some(tip) = commit_of(&odb, tip)? else {
            return Ok(Vec::new());
        };
        painter.paint(tip, FROM_TIP)?;
        while !painter.done() {
            let Some((_, id)) = painter.queue.pop() else {
                break;
            };
            let (flags, _, commit) = painter.seen[&id].clone();
            for &parent in &commit.parents {
                painter.paint(parent, flags)?;
            }
        }
        Ok(painter
            .only_tip
            .iter()
            .rev()
            .map(|(_, id)| (*id, painter.seen[id].2.clone()))
            .collect())
    }

    /// The files a commit writes against its first parent, or every file of a root commit,
    /// each with its path and blob. Removed files are left out.
    ///
    /// # Errors
    ///
    /// [`GitError::NotFound`] when a commit or tree is missing.
    pub fn written_files(
        &self,
        tx: &Tx,
        repo: i64,
        commit: &Commit,
    ) -> Result<Vec<(String, ObjectId)>, GitError> {
        let odb = self.odb(tx, repo);
        let parent = commit
            .parents
            .first()
            .map(|p| odb.commit(*p).map(|c| c.tree))
            .transpose()?;
        Ok(tree_changes(&odb, parent, Some(commit.tree))?
            .into_iter()
            .filter_map(|c| c.new.map(|(_, id)| (c.path, id)))
            .collect())
    }

    /// The paths that differ between the merge base of `base` and `head` and `head`, as the
    /// files of GitHub's three-dot compare and of a pull request, without their patches.
    ///
    /// # Errors
    ///
    /// [`GitError::NotFound`] when either commit is missing.
    pub fn changed_paths(
        &self,
        tx: &Tx,
        repo: i64,
        base: ObjectId,
        head: ObjectId,
    ) -> Result<Vec<String>, GitError> {
        let odb = self.odb(tx, repo);
        let (base, base_commit) = odb.peel_commit(base)?;
        let (head, head_commit) = odb.peel_commit(head)?;
        let from = match merge_base(&odb, base, head)? {
            Some(m) => odb.commit(m)?.tree,
            None => base_commit.tree,
        };
        Ok(tree_changes(&odb, Some(from), Some(head_commit.tree))?
            .into_iter()
            .map(|c| c.path)
            .collect())
    }

    /// The size in bytes of a blob.
    ///
    /// # Errors
    ///
    /// [`GitError::NotFound`] for a missing blob.
    pub fn blob_size(&self, tx: &Tx, repo: i64, id: ObjectId) -> Result<usize, GitError> {
        Ok(self.odb(tx, repo).blob(id)?.len())
    }

    /// Returns the commits a branch update makes reachable, with parents before children.
    ///
    /// # Errors
    ///
    /// Returns [`GitError::NotFound`] if the repository lacks a commit in the history.
    pub fn newly_reachable_commits(
        &self,
        tx: &Tx,
        change: &RefChange,
    ) -> Result<Vec<(ObjectId, Arc<Commit>)>, GitError> {
        let Some(head) = change.new else {
            return Ok(Vec::new());
        };
        let odb = self.odb(tx, change.repo_id);
        let mut tip = head;
        let mut suffix = Vec::new();
        if let Some(old) = change.old.filter(|_| !change.forced) {
            while tip != old {
                let commit = odb.commit(tip)?;
                let [parent] = commit.parents.as_slice() else {
                    break;
                };
                let id = tip;
                tip = *parent;
                suffix.push((id, commit));
            }
            if tip == old {
                suffix.reverse();
                return Ok(suffix);
            }
        }
        let known = change
            .old
            .map(|old| ancestors(&odb, old))
            .transpose()?
            .unwrap_or_default();
        let mut commits = topo(&odb, tip, &known)?;
        commits.extend(suffix.into_iter().rev());
        Ok(commits)
    }
}

fn commit_of(odb: &Odb, id: ObjectId) -> Result<Option<ObjectId>, GitError> {
    match odb.peel_commit(id) {
        Ok((commit, _)) => Ok(Some(commit)),
        Err(GitError::WrongKind) => Ok(None),
        Err(error) => Err(error),
    }
}

pub(crate) fn ancestors(odb: &Odb, start: ObjectId) -> Result<HashSet<ObjectId>, GitError> {
    let mut seen = HashSet::from([start]);
    let mut stack = vec![start];
    while let Some(id) = stack.pop() {
        for parent in &odb.commit(id)?.parents {
            if seen.insert(*parent) {
                stack.push(*parent);
            }
        }
    }
    Ok(seen)
}

pub(crate) fn is_ancestor(odb: &Odb, ancestor: ObjectId, of: ObjectId) -> Result<bool, GitError> {
    if ancestor == of {
        return Ok(true);
    }
    let mut seen = HashSet::from([of]);
    let mut stack = vec![of];
    while let Some(id) = stack.pop() {
        for parent in &odb.commit(id)?.parents {
            if *parent == ancestor {
                return Ok(true);
            }
            if seen.insert(*parent) {
                stack.push(*parent);
            }
        }
    }
    Ok(false)
}

pub(crate) fn fast_forward(odb: &Odb, old: ObjectId, new: ObjectId) -> Result<bool, GitError> {
    match (odb.get(old)?, odb.get(new)?) {
        (ObjectData::Commit(_), ObjectData::Commit(_)) => is_ancestor(odb, old, new),
        _ => Ok(false),
    }
}

pub(crate) fn best_common(
    odb: &Odb,
    a: &HashSet<ObjectId>,
    b: &HashSet<ObjectId>,
) -> Result<Option<ObjectId>, GitError> {
    let common: Vec<ObjectId> = a.intersection(b).copied().collect();
    let mut covered = HashSet::new();
    for id in &common {
        covered.extend(odb.commit(*id)?.parents.iter().copied());
    }
    let mut best = None;
    for id in common.into_iter().filter(|id| !covered.contains(id)) {
        let time = odb.commit(id)?.committer.time;
        if best.is_none_or(|(t, b)| (time, id) > (t, b)) {
            best = Some((time, id));
        }
    }
    Ok(best.map(|(_, id)| id))
}

pub(crate) fn merge_base(
    odb: &Odb,
    a: ObjectId,
    b: ObjectId,
) -> Result<Option<ObjectId>, GitError> {
    best_common(odb, &ancestors(odb, a)?, &ancestors(odb, b)?)
}

pub(crate) fn topo(
    odb: &Odb,
    head: ObjectId,
    exclude: &HashSet<ObjectId>,
) -> Result<Vec<(ObjectId, Arc<Commit>)>, GitError> {
    let mut out = Vec::new();
    if exclude.contains(&head) {
        return Ok(out);
    }
    let mut seen = HashSet::from([head]);
    let mut stack = vec![(head, odb.commit(head)?, 0usize)];
    while let Some((id, commit, next)) = stack.pop() {
        match commit.parents.get(next).copied() {
            Some(parent) => {
                stack.push((id, commit, next + 1));
                if !exclude.contains(&parent) && seen.insert(parent) {
                    stack.push((parent, odb.commit(parent)?, 0));
                }
            }
            None => out.push((id, commit)),
        }
    }
    Ok(out)
}

pub(crate) fn compare_order(
    odb: &Odb,
    head: ObjectId,
    exclude: &HashSet<ObjectId>,
) -> Result<Vec<(ObjectId, Arc<Commit>)>, GitError> {
    if exclude.contains(&head) {
        return Ok(Vec::new());
    }
    let mut seen = HashSet::from([head]);
    let mut queue = BinaryHeap::from([(odb.commit(head)?.committer.time, Reverse(1), head)]);
    let mut commits = Vec::new();
    while let Some((_, _, id)) = queue.pop() {
        let commit = odb.commit(id)?;
        for &parent in &commit.parents {
            if !exclude.contains(&parent) && seen.insert(parent) {
                let time = odb.commit(parent)?.committer.time;
                queue.push((time, Reverse(seen.len()), parent));
            }
        }
        commits.push((id, commit));
    }
    commits.reverse();
    Ok(commits)
}

pub(crate) fn log(
    odb: &Odb,
    head: ObjectId,
    first_parent: bool,
    limit: usize,
) -> Result<Vec<(ObjectId, Arc<Commit>)>, GitError> {
    let mut out = Vec::new();
    if first_parent {
        let mut next = Some(head);
        while let Some(id) = next.filter(|_| out.len() < limit) {
            let commit = odb.commit(id)?;
            next = commit.parents.first().copied();
            out.push((id, commit));
        }
        return Ok(out);
    }
    let first = odb.commit(head)?;
    let mut seen = HashSet::from([head]);
    let mut queue = BinaryHeap::from([(first.committer.time, head)]);
    while let Some((_, id)) = queue.pop() {
        if out.len() >= limit {
            break;
        }
        let commit = odb.commit(id)?;
        for parent in &commit.parents {
            if seen.insert(*parent) {
                queue.push((odb.commit(*parent)?.committer.time, *parent));
            }
        }
        out.push((id, commit));
    }
    Ok(out)
}
