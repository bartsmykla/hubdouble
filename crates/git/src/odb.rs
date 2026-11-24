use std::collections::HashMap;
use std::sync::Arc;

use foldhash::fast::FixedState;
use parking_lot::RwLock;

use crate::contract::{Commit, EntryMode, GitError, ObjectData, ObjectId, TreeEntry};
use crate::encode;
use crate::tree::check_tree_depth;

const SHARDS: usize = 16;
const MAX_PEEL: usize = 16;

struct StoredObject {
    data: ObjectData,
    tree_depth: usize,
    generation: u64,
}

type Objects = HashMap<ObjectId, StoredObject, FixedState>;

#[derive(Default)]
pub(crate) struct Odb {
    shards: [RwLock<Objects>; SHARDS],
}

impl Odb {
    fn shard(&self, id: ObjectId) -> &RwLock<Objects> {
        &self.shards[usize::from(id.0[0]) % SHARDS]
    }

    pub(crate) fn get(&self, id: ObjectId) -> Result<ObjectData, GitError> {
        self.shard(id)
            .read()
            .get(&id)
            .map(|object| object.data.clone())
            .ok_or(GitError::NotFound)
    }

    pub(crate) fn len(&self) -> usize {
        self.shards.iter().map(|s| s.read().len()).sum()
    }

    pub(crate) fn contains(&self, id: ObjectId) -> bool {
        self.shard(id).read().contains_key(&id)
    }

    pub(crate) fn put(&self, data: ObjectData) -> Result<ObjectId, GitError> {
        let mut tree_depth = 0;
        if let ObjectData::Tree(entries) = &data {
            for entry in entries.iter().filter(|entry| entry.mode == EntryMode::Tree) {
                tree_depth = tree_depth.max(self.tree_depth(entry.id)? + 1);
            }
            check_tree_depth(tree_depth)?;
        }
        let generation = match &data {
            ObjectData::Commit(c) => c
                .parents
                .iter()
                .map(|p| self.generation(*p))
                .max()
                .map_or(1, |g| g.saturating_add(1)),
            _ => 0,
        };
        let id = encode::hash(&data)?;
        self.shard(id).write().entry(id).or_insert(StoredObject {
            data,
            tree_depth,
            generation,
        });
        Ok(id)
    }

    pub(crate) fn generation(&self, id: ObjectId) -> u64 {
        self.shard(id)
            .read()
            .get(&id)
            .map_or(u64::MAX, |object| object.generation)
    }

    fn tree_depth(&self, id: ObjectId) -> Result<usize, GitError> {
        let shard = self.shard(id).read();
        let object = shard.get(&id).ok_or(GitError::NotFound)?;
        match object.data {
            ObjectData::Tree(_) => Ok(object.tree_depth),
            _ => Err(GitError::WrongKind),
        }
    }

    pub(crate) fn commit(&self, id: ObjectId) -> Result<Arc<Commit>, GitError> {
        match self.get(id)? {
            ObjectData::Commit(c) => Ok(c),
            _ => Err(GitError::WrongKind),
        }
    }

    pub(crate) fn tree(&self, id: ObjectId) -> Result<Arc<[TreeEntry]>, GitError> {
        match self.get(id)? {
            ObjectData::Tree(t) => Ok(t),
            _ => Err(GitError::WrongKind),
        }
    }

    pub(crate) fn blob(&self, id: ObjectId) -> Result<Arc<[u8]>, GitError> {
        match self.get(id)? {
            ObjectData::Blob(b) => Ok(b),
            _ => Err(GitError::WrongKind),
        }
    }

    pub(crate) fn peel_commit(
        &self,
        mut id: ObjectId,
    ) -> Result<(ObjectId, Arc<Commit>), GitError> {
        for _ in 0..MAX_PEEL {
            match self.get(id)? {
                ObjectData::Commit(c) => return Ok((id, c)),
                ObjectData::Tag(t) => id = t.target,
                _ => return Err(GitError::WrongKind),
            }
        }
        Err(GitError::WrongKind)
    }
}
