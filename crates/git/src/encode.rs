use gix_object::bstr::BString;
use gix_object::tree::EntryKind;
use gix_object::{Kind, WriteTo};

use crate::contract::{
    Commit, EntryMode, GitError, ObjectData, ObjectId, ObjectKind, Signature, Tag, TreeEntry,
};

fn oid(id: ObjectId) -> gix_hash::ObjectId {
    gix_hash::ObjectId::Sha1(id.0)
}

fn actor(s: &Signature) -> gix_actor::Signature {
    gix_actor::Signature {
        name: s.name.as_str().into(),
        email: s.email.as_str().into(),
        time: gix_object::date::Time {
            seconds: s.time.unix(),
            offset: s.offset_minutes * 60,
        },
    }
}

fn kind(k: ObjectKind) -> Kind {
    match k {
        ObjectKind::Blob => Kind::Blob,
        ObjectKind::Tree => Kind::Tree,
        ObjectKind::Commit => Kind::Commit,
        ObjectKind::Tag => Kind::Tag,
    }
}

fn mode(m: EntryMode) -> gix_object::tree::EntryMode {
    match m {
        EntryMode::File => EntryKind::Blob,
        EntryMode::Executable => EntryKind::BlobExecutable,
        EntryMode::Symlink => EntryKind::Link,
        EntryMode::Tree => EntryKind::Tree,
        EntryMode::Submodule => EntryKind::Commit,
    }
    .into()
}

fn tree(entries: &[TreeEntry]) -> gix_object::Tree {
    gix_object::Tree {
        entries: entries
            .iter()
            .map(|e| gix_object::tree::Entry {
                mode: mode(e.mode),
                filename: e.name.as_str().into(),
                oid: oid(e.id),
            })
            .collect(),
    }
}

fn commit(c: &Commit) -> gix_object::Commit {
    gix_object::Commit {
        tree: oid(c.tree),
        parents: c.parents.iter().copied().map(oid).collect(),
        author: actor(&c.author),
        committer: actor(&c.committer),
        encoding: None,
        message: c.message.as_str().into(),
        extra_headers: c
            .gpg_signature
            .iter()
            .map(|s| (BString::from("gpgsig"), BString::from(s.as_str())))
            .collect(),
    }
}

fn tag(t: &Tag) -> gix_object::Tag {
    gix_object::Tag {
        target: oid(t.target),
        target_kind: kind(t.target_kind),
        name: t.name.as_str().into(),
        tagger: t.tagger.as_ref().map(actor),
        message: t.message.as_str().into(),
        signature: t.gpg_signature.as_deref().map(BString::from),
    }
}

fn write(object: &gix_object::Object) -> Result<Vec<u8>, GitError> {
    let mut out = Vec::new();
    object
        .write_to(&mut out)
        .map_err(|e| GitError::Invalid(e.to_string()))?;
    Ok(out)
}

pub(crate) fn commit_bytes(c: &Commit) -> Result<Vec<u8>, GitError> {
    write(&gix_object::Object::Commit(commit(c)))
}

pub(crate) fn hash(data: &ObjectData) -> Result<ObjectId, GitError> {
    let (k, bytes) = match data {
        ObjectData::Blob(b) => return hash_bytes(Kind::Blob, b),
        ObjectData::Tree(t) => (Kind::Tree, write(&gix_object::Object::Tree(tree(t)))?),
        ObjectData::Commit(c) => (Kind::Commit, commit_bytes(c)?),
        ObjectData::Tag(t) => (Kind::Tag, write(&gix_object::Object::Tag(tag(t)))?),
    };
    hash_bytes(k, &bytes)
}

fn hash_bytes(k: Kind, bytes: &[u8]) -> Result<ObjectId, GitError> {
    let id = gix_object::compute_hash(gix_hash::Kind::Sha1, k, bytes)
        .map_err(|e| GitError::Invalid(e.to_string()))?;
    let mut out = [0u8; 20];
    if id.as_slice().len() != out.len() {
        return Err(GitError::BadObjectId);
    }
    out.copy_from_slice(id.as_slice());
    Ok(ObjectId(out))
}
