use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::RwLock;

use crate::contract::{Commit, GitError, ObjectData, ObjectId, TreeEntry};
use crate::encode;

const SHARDS: usize = 16;
const MAX_PEEL: usize = 16;

#[derive(Default)]
pub(crate) struct Odb {
    shards: [RwLock<HashMap<ObjectId, ObjectData>>; SHARDS],
}

impl Odb {
    fn shard(&self, id: ObjectId) -> &RwLock<HashMap<ObjectId, ObjectData>> {
        &self.shards[usize::from(id.0[0]) % SHARDS]
    }

    pub(crate) fn get(&self, id: ObjectId) -> Result<ObjectData, GitError> {
        self.shard(id)
            .read()
            .get(&id)
            .cloned()
            .ok_or(GitError::NotFound)
    }

    pub(crate) fn contains(&self, id: ObjectId) -> bool {
        self.shard(id).read().contains_key(&id)
    }

    pub(crate) fn put(&self, data: ObjectData) -> Result<ObjectId, GitError> {
        let id = encode::hash(&data)?;
        self.shard(id).write().entry(id).or_insert(data);
        Ok(id)
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
