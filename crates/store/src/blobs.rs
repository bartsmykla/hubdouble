use std::collections::HashMap;
use std::sync::Arc;

use bytes::Bytes;
use hmac::{Hmac, KeyInit, Mac};
use hubdouble_model::Timestamp;
use sha2::Sha256;

use crate::store::{Table, TableMut};

/// Bytes served at `/_blobs/{id}`: job logs and release assets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Blob {
    pub content_type: String,
    pub data: Bytes,
    /// The key that signs links to the blob when each link expires, as a job log link does;
    /// `None` serves the blob at its bare path.
    pub link_key: Option<Arc<str>>,
}

impl Blob {
    /// The signature of a link to the blob `id` that expires at `expiry`, or `None` for a blob
    /// whose links do not expire.
    #[must_use]
    pub fn link_signature(&self, id: &str, expiry: Timestamp) -> Option<String> {
        let key = self.link_key.as_deref()?;
        let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(key.as_bytes()).ok()?;
        mac.update(id.as_bytes());
        mac.update(&expiry.unix().to_be_bytes());
        let tag = mac.finalize().into_bytes();
        Some(tag.iter().fold(String::with_capacity(64), |mut s, b| {
            s.push(char::from(HEX[usize::from(b >> 4)]));
            s.push(char::from(HEX[usize::from(b & 0xf)]));
            s
        }))
    }
}

const HEX: &[u8; 16] = b"0123456789abcdef";

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
