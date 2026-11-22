use std::collections::HashMap;
use std::sync::Arc;

use bytes::Bytes;

use crate::store::{Table, TableMut};

/// Bytes served at `/_blobs/{id}`: job logs and release assets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Blob {
    pub content_type: String,
    pub data: Bytes,
}

#[derive(Debug, Clone, Default)]
pub struct Blobs {
    blobs: HashMap<String, Arc<Blob>>,
}

impl Table for Blobs {}

impl Blobs {
    #[must_use]
    pub fn get(&self, id: &str) -> Option<&Arc<Blob>> {
        self.blobs.get(id)
    }
}

impl TableMut<'_, Blobs> {
    pub fn insert(&mut self, id: String, blob: Blob) {
        let old = self.table.blobs.insert(id.clone(), Arc::new(blob));
        self.record(move |b| match old {
            Some(old) => {
                b.blobs.insert(id, old);
            }
            None => {
                b.blobs.remove(&id);
            }
        });
    }

    pub fn remove(&mut self, id: &str) -> Option<Arc<Blob>> {
        let old = self.table.blobs.remove(id)?;
        let (id, kept) = (id.to_owned(), old.clone());
        self.record(move |b| {
            b.blobs.insert(id, kept);
        });
        Some(old)
    }
}
