use std::io::{self, Write as _};

use flate2::write::{DeflateEncoder, GzEncoder};
use flate2::{Compression, Crc};
use hubdouble_model::{Timestamp, civil_from_days};
use hubdouble_store::Tx;

use crate::{EntryMode, Git, GitError, ObjectData, ObjectId};

/// The formats of GitHub's `tarball` and `zipball` and of migration archives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveFormat {
    /// A gzip-compressed tar file.
    TarGz,
    Zip,
}

/// An archive written entry by entry with the modes and owner `git archive` writes: directories
/// and executables `0775`, files `0664`, symlinks `0777`, owner `root`, and one `mtime` for every
/// entry. A zip archive refuses more than 65535 entries or 4 GiB, which needs zip64.
pub struct Archive {
    mtime: Timestamp,
    out: Out,
}

enum Out {
    Tar(tar::Builder<GzEncoder<Vec<u8>>>),
    Zip(Zip),
}

struct Zip {
    bytes: Vec<u8>,
    central: Vec<u8>,
    entries: u16,
    comment: Vec<u8>,
    time: u16,
    date: u16,
}

fn write_failed(e: &io::Error) -> GitError {
    GitError::Compress(e.to_string())
}

fn fits<T: TryFrom<usize>>(n: usize) -> Result<T, GitError> {
    T::try_from(n).map_err(|_| GitError::Invalid("the archive needs zip64".to_owned()))
}

impl Archive {
    /// An empty archive. `comment` is the pax global `comment` of a tar file and the archive
    /// comment of a zip file, where `git archive` writes the commit ID.
    ///
    /// # Errors
    ///
    /// [`GitError::Compress`] when the tar header cannot be written.
    pub fn new(
        format: ArchiveFormat,
        mtime: Timestamp,
        comment: Option<&str>,
    ) -> Result<Archive, GitError> {
        let out = match format {
            ArchiveFormat::TarGz => {
                let mut tar = tar::Builder::new(GzEncoder::new(Vec::new(), Compression::default()));
                if let Some(comment) = comment {
                    let body = pax_record("comment", comment);
                    let mut h = tar::Header::new_ustar();
                    h.set_entry_type(tar::EntryType::XGlobalHeader);
                    h.set_size(body.len() as u64);
                    h.set_mode(0o666);
                    h.set_mtime(mtime_of(mtime));
                    tar.append_data(&mut h, "pax_global_header", body.as_bytes())
                        .map_err(|e| write_failed(&e))?;
                }
                Out::Tar(tar)
            }
            ArchiveFormat::Zip => {
                let (time, date) = dos_time(mtime);
                Out::Zip(Zip {
                    bytes: Vec::new(),
                    central: Vec::new(),
                    entries: 0,
                    comment: comment.unwrap_or_default().as_bytes().to_vec(),
                    time,
                    date,
                })
            }
        };
        Ok(Archive { mtime, out })
    }

    /// Adds a directory; `path` ends with `/`.
    ///
    /// # Errors
    ///
    /// As [`Archive::finish`].
    pub fn dir(&mut self, path: &str) -> Result<(), GitError> {
        self.add(path, b"", 0o040_775, tar::EntryType::Directory)
    }

    /// Adds a file, `0775` when `executable`.
    ///
    /// # Errors
    ///
    /// As [`Archive::finish`].
    pub fn file(&mut self, path: &str, data: &[u8], executable: bool) -> Result<(), GitError> {
        let mode = if executable { 0o100_775 } else { 0o100_664 };
        self.add(path, data, mode, tar::EntryType::Regular)
    }

    /// Adds a symbolic link to `target`.
    ///
    /// # Errors
    ///
    /// As [`Archive::finish`].
    pub fn symlink(&mut self, path: &str, target: &str) -> Result<(), GitError> {
        self.add(path, target.as_bytes(), 0o120_777, tar::EntryType::Symlink)
    }

    fn add(
        &mut self,
        path: &str,
        data: &[u8],
        mode: u32,
        kind: tar::EntryType,
    ) -> Result<(), GitError> {
        match &mut self.out {
            Out::Tar(tar) => {
                let mut h = tar::Header::new_gnu();
                h.set_entry_type(kind);
                h.set_mode(mode & 0o7777);
                h.set_mtime(mtime_of(self.mtime));
                h.set_uid(0);
                h.set_gid(0);
                h.set_username("root").map_err(|e| write_failed(&e))?;
                h.set_groupname("root").map_err(|e| write_failed(&e))?;
                let written = if kind == tar::EntryType::Symlink {
                    h.set_size(0);
                    tar.append_link(&mut h, path, String::from_utf8_lossy(data).as_ref())
                } else {
                    h.set_size(data.len() as u64);
                    tar.append_data(&mut h, path, data)
                };
                written.map_err(|e| write_failed(&e))
            }
            Out::Zip(zip) => zip.add(path, data, mode),
        }
    }

    /// The archive's bytes.
    ///
    /// # Errors
    ///
    /// [`GitError::Compress`] when compression fails, and [`GitError::Invalid`] for a zip archive
    /// that needs zip64.
    pub fn finish(self) -> Result<Vec<u8>, GitError> {
        match self.out {
            Out::Tar(tar) => tar
                .into_inner()
                .and_then(GzEncoder::finish)
                .map_err(|e| write_failed(&e)),
            Out::Zip(zip) => zip.finish(),
        }
    }
}

impl Zip {
    fn add(&mut self, name: &str, data: &[u8], mode: u32) -> Result<(), GitError> {
        let mut crc = Crc::new();
        crc.update(data);
        let deflated = if data.is_empty() || mode & 0o170_000 != 0o100_000 {
            None
        } else {
            let mut e = DeflateEncoder::new(Vec::new(), Compression::default());
            e.write_all(data).map_err(|e| write_failed(&e))?;
            Some(e.finish().map_err(|e| write_failed(&e))?)
        };
        let (method, body): (u16, &[u8]) = match &deflated {
            Some(d) if d.len() < data.len() => (8, d),
            _ => (0, data),
        };
        let offset: u32 = fits(self.bytes.len())?;
        let name_len: u16 = fits(name.len())?;
        let (stored, size): (u32, u32) = (fits(body.len())?, fits(data.len())?);
        let flags: u16 = if name.is_ascii() { 0 } else { 0x0800 };
        let common = |out: &mut Vec<u8>| {
            for half in [20, flags, method, self.time, self.date] {
                out.extend_from_slice(&half.to_le_bytes());
            }
            for word in [crc.sum(), stored, size] {
                out.extend_from_slice(&word.to_le_bytes());
            }
            out.extend_from_slice(&name_len.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
        };
        self.bytes.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        common(&mut self.bytes);
        self.bytes.extend_from_slice(name.as_bytes());
        self.bytes.extend_from_slice(body);
        let dos_dir = if mode & 0o170_000 == 0o040_000 {
            0x10
        } else {
            0
        };
        self.central
            .extend_from_slice(&0x0201_4b50u32.to_le_bytes());
        self.central.extend_from_slice(&0x0314u16.to_le_bytes());
        common(&mut self.central);
        self.central.extend_from_slice(&[0; 6]);
        self.central
            .extend_from_slice(&(mode << 16 | dos_dir).to_le_bytes());
        self.central.extend_from_slice(&offset.to_le_bytes());
        self.central.extend_from_slice(name.as_bytes());
        self.entries = self
            .entries
            .checked_add(1)
            .ok_or_else(|| GitError::Invalid("the archive needs zip64".to_owned()))?;
        Ok(())
    }

    fn finish(mut self) -> Result<Vec<u8>, GitError> {
        let offset: u32 = fits(self.bytes.len())?;
        let size: u32 = fits(self.central.len())?;
        let comment_len: u16 = fits(self.comment.len())?;
        self.bytes.append(&mut self.central);
        self.bytes.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
        for half in [0, 0, self.entries, self.entries] {
            self.bytes.extend_from_slice(&half.to_le_bytes());
        }
        self.bytes.extend_from_slice(&size.to_le_bytes());
        self.bytes.extend_from_slice(&offset.to_le_bytes());
        self.bytes.extend_from_slice(&comment_len.to_le_bytes());
        self.bytes.extend_from_slice(&self.comment);
        Ok(self.bytes)
    }
}

fn pax_record(key: &str, value: &str) -> String {
    let body = format!(" {key}={value}\n");
    let mut len = body.len() + 1;
    while body.len() + len.to_string().len() != len {
        len = body.len() + len.to_string().len();
    }
    format!("{len}{body}")
}

fn mtime_of(t: Timestamp) -> u64 {
    u64::try_from(t.unix()).unwrap_or(0)
}

fn dos_time(t: Timestamp) -> (u16, u16) {
    let secs = t.unix();
    let (y, m, d) = civil_from_days(secs.div_euclid(86_400));
    let in_day = secs.rem_euclid(86_400);
    let Ok(years) = u16::try_from(y - 1980) else {
        return (0, 1 << 5 | 1);
    };
    let part = |n: i64| u16::try_from(n).unwrap_or(0);
    let time = part(in_day / 3600) << 11 | part(in_day / 60 % 60) << 5 | part(in_day % 60 / 2);
    (time, years.min(127) << 9 | part(m) << 5 | part(d))
}

/// GitHub's `tarball` or `zipball` of a commit-ish: every path under `{prefix}/`, with the
/// committer time as each entry's time and the commit ID as the archive comment. Submodules are
/// empty directories, as `git archive` writes them.
///
/// # Errors
///
/// [`GitError::NotFound`] or [`GitError::WrongKind`] when `id` does not peel to a commit, and as
/// [`Archive::finish`].
pub fn archive_commit(
    git: &Git,
    tx: &Tx,
    repo: i64,
    id: ObjectId,
    prefix: &str,
    format: ArchiveFormat,
) -> Result<Vec<u8>, GitError> {
    let commit = git.peel_to_commit(tx, repo, id)?;
    let ObjectData::Commit(c) = git.object(tx, repo, commit)?.data else {
        return Err(GitError::WrongKind);
    };
    let mut archive = Archive::new(format, c.committer.time, Some(&commit.to_string()))?;
    let root = format!("{prefix}/");
    archive.dir(&root)?;
    add_tree(git, tx, repo, c.tree, &root, &mut archive)?;
    archive.finish()
}

fn add_tree(
    git: &Git,
    tx: &Tx,
    repo: i64,
    tree: ObjectId,
    base: &str,
    archive: &mut Archive,
) -> Result<(), GitError> {
    let ObjectData::Tree(entries) = git.object(tx, repo, tree)?.data else {
        return Err(GitError::WrongKind);
    };
    for e in entries.iter() {
        let path = format!("{base}{}", e.name);
        match e.mode {
            EntryMode::Tree => {
                let dir = format!("{path}/");
                archive.dir(&dir)?;
                add_tree(git, tx, repo, e.id, &dir, archive)?;
            }
            EntryMode::Submodule => archive.dir(&format!("{path}/"))?,
            EntryMode::File | EntryMode::Executable | EntryMode::Symlink => {
                let ObjectData::Blob(data) = git.object(tx, repo, e.id)?.data else {
                    return Err(GitError::WrongKind);
                };
                match e.mode {
                    EntryMode::Symlink => {
                        archive.symlink(&path, &String::from_utf8_lossy(&data))?;
                    }
                    mode => archive.file(&path, &data, mode == EntryMode::Executable)?,
                }
            }
        }
    }
    Ok(())
}
