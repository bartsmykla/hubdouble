use std::collections::HashSet;
use std::fmt::Write as _;
use std::io::Read as _;
use std::sync::Arc;

use flate2::read::ZlibDecoder;
use flate2::{Compress, Compression, FlushCompress, Status};
use git2::{DiffBinary, DiffBinaryFile, DiffBinaryKind, DiffDelta, DiffOptions, Repository};
use hubdouble_model::civil_from_days;

use crate::contract::{Commit, EntryMode, GitError, ObjectId, Signature};
use crate::diff::{Change, Leaf, renames, text, text_patch};
use crate::odb::Odb;

const STAT_WIDTH: usize = 72;
const MAIL_WIDTH: usize = 78;
const ENCODED_WIDTH: usize = 76;
const MIN_ABBREV: usize = 7;
const BINARY_LINE: usize = 52;
const BASE85: &[u8; 85] =
    b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz!#$%&()*+-;<=>?@^_`{|}~";
const DAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

struct Side {
    path: String,
    mode: EntryMode,
    id: ObjectId,
    data: Arc<[u8]>,
}

pub(crate) struct File {
    old: Option<Side>,
    new: Option<Side>,
    binary: bool,
    hunks: Option<String>,
    added: u64,
    deleted: u64,
}

pub(crate) fn abbrev(objects: usize) -> usize {
    usize::try_from(objects.bit_width().max(1).div_ceil(2))
        .unwrap_or(MIN_ABBREV)
        .max(MIN_ABBREV)
}

fn data(odb: &Odb, (mode, id): Leaf) -> Result<Arc<[u8]>, GitError> {
    if mode == EntryMode::Submodule {
        return Ok(Arc::from(format!("Subproject commit {id}\n").into_bytes()));
    }
    odb.blob(id)
}

pub(crate) fn files(odb: &Odb, changes: &[Change]) -> Result<Vec<File>, GitError> {
    let renamed = renames(changes);
    let consumed: HashSet<usize> = renamed.values().copied().collect();
    let side = |c: &Change, leaf: Option<Leaf>| -> Result<Option<Side>, GitError> {
        leaf.map(|leaf| {
            Ok(Side {
                path: c.path.clone(),
                mode: leaf.0,
                id: leaf.1,
                data: data(odb, leaf)?,
            })
        })
        .transpose()
    };
    let mut out = Vec::with_capacity(changes.len());
    for (i, c) in changes.iter().enumerate() {
        if consumed.contains(&i) {
            continue;
        }
        let old = match renamed.get(&i) {
            Some(&from) => side(&changes[from], changes[from].old)?,
            None => side(c, c.old)?,
        };
        out.push(File::new(old, side(c, c.new)?));
    }
    out.sort_by(|a, b| a.path().cmp(b.path()));
    Ok(out)
}

impl File {
    fn new(old: Option<Side>, new: Option<Side>) -> File {
        let empty: &[u8] = &[];
        let a = old.as_ref().map_or(empty, |s| &s.data);
        let b = new.as_ref().map_or(empty, |s| &s.data);
        let mut file = File {
            binary: false,
            hunks: None,
            added: 0,
            deleted: 0,
            old: None,
            new: None,
        };
        if let (Some(x), Some(y)) = (text(a), text(b)) {
            (file.hunks, file.added, file.deleted) = text_patch(x, y);
        } else {
            file.binary = true;
            if a != b {
                file.added = len(b);
                file.deleted = len(a);
            }
        }
        file.old = old;
        file.new = new;
        file
    }

    fn path(&self) -> &str {
        self.new
            .as_ref()
            .or(self.old.as_ref())
            .map_or("", |s| &s.path)
    }

    fn old_path(&self) -> &str {
        self.old
            .as_ref()
            .or(self.new.as_ref())
            .map_or("", |s| &s.path)
    }

    fn renamed(&self) -> bool {
        self.old.is_some() && self.new.is_some() && self.old_path() != self.path()
    }

    fn modes(&self) -> Option<(EntryMode, EntryMode)> {
        let (old, new) = (self.old.as_ref()?, self.new.as_ref()?);
        (old.mode != new.mode).then_some((old.mode, new.mode))
    }

    fn write(&self, out: &mut String, abbrev: usize, binary_patch: bool) {
        let _ = writeln!(
            out,
            "diff --git {} {}",
            quote("a/", self.old_path()),
            quote("b/", self.path())
        );
        match (&self.old, &self.new, self.modes()) {
            (None, Some(new), _) => {
                let _ = writeln!(out, "new file mode {}", new.mode.as_str());
            }
            (Some(old), None, _) => {
                let _ = writeln!(out, "deleted file mode {}", old.mode.as_str());
            }
            (_, _, Some((a, b))) => {
                let _ = writeln!(out, "old mode {}\nnew mode {}", a.as_str(), b.as_str());
            }
            _ => {}
        }
        if self.renamed() {
            let _ = writeln!(
                out,
                "similarity index 100%\nrename from {}\nrename to {}",
                quote("", self.old_path()),
                quote("", self.path())
            );
        }
        let old_id = self.old.as_ref().map_or(ObjectId::NULL, |s| s.id);
        let new_id = self.new.as_ref().map_or(ObjectId::NULL, |s| s.id);
        if old_id == new_id {
            return;
        }
        let width = if binary_patch && self.binary {
            40
        } else {
            abbrev
        };
        let (a, b) = (old_id.to_string(), new_id.to_string());
        let _ = write!(out, "index {}..{}", &a[..width], &b[..width]);
        if let (Some(old), Some(new)) = (&self.old, &self.new)
            && old.mode == new.mode
        {
            let _ = write!(out, " {}", new.mode.as_str());
        }
        out.push('\n');
        let label = |side: &Option<Side>, prefix: &str| {
            side.as_ref()
                .map_or_else(|| "/dev/null".to_owned(), |s| quote(prefix, &s.path))
        };
        if self.binary && binary_patch {
            let empty: &[u8] = &[];
            binary_body(
                out,
                self.old.as_ref().map_or(empty, |s| &s.data),
                self.new.as_ref().map_or(empty, |s| &s.data),
            );
        } else if self.binary {
            let _ = writeln!(
                out,
                "Binary files {} and {} differ",
                label(&self.old, "a/"),
                label(&self.new, "b/")
            );
        } else if let Some(hunks) = &self.hunks {
            let _ = writeln!(
                out,
                "--- {}\n+++ {}\n{hunks}",
                label(&self.old, "a/"),
                label(&self.new, "b/")
            );
        }
    }

    fn stat_name(&self) -> String {
        if self.renamed() {
            rename_name(self.old_path(), self.path())
        } else {
            quote("", self.path())
        }
    }
}

fn len(bytes: &[u8]) -> u64 {
    u64::try_from(bytes.len()).unwrap_or(u64::MAX)
}

fn needs_quote(path: &str) -> bool {
    path.bytes()
        .any(|b| !(0x20..0x7f).contains(&b) || b == b'"' || b == b'\\')
}

fn quote(prefix: &str, path: &str) -> String {
    if !needs_quote(path) {
        return format!("{prefix}{path}");
    }
    let mut out = format!("\"{prefix}");
    for b in path.bytes() {
        match b {
            0x07 => out.push_str("\\a"),
            0x08 => out.push_str("\\b"),
            b'\t' => out.push_str("\\t"),
            b'\n' => out.push_str("\\n"),
            0x0b => out.push_str("\\v"),
            0x0c => out.push_str("\\f"),
            b'\r' => out.push_str("\\r"),
            b'"' => out.push_str("\\\""),
            b'\\' => out.push_str("\\\\"),
            0x20..=0x7e => out.push(char::from(b)),
            _ => {
                let _ = write!(out, "\\{b:03o}");
            }
        }
    }
    out.push('"');
    out
}

fn rename_name(a: &str, b: &str) -> String {
    if needs_quote(a) || needs_quote(b) {
        return format!("{} => {}", quote("", a), quote("", b));
    }
    let (ab, bb) = (a.as_bytes(), b.as_bytes());
    let mut prefix = 0;
    for (i, (x, y)) in ab.iter().zip(bb).enumerate() {
        if x != y {
            break;
        }
        if *x == b'/' {
            prefix = i + 1;
        }
    }
    let floor = prefix.saturating_sub(usize::from(prefix > 0));
    let mut suffix = 0;
    let (mut i, mut j) = (ab.len(), bb.len());
    while i > floor && j > floor && ab[i - 1] == bb[j - 1] {
        if ab[i - 1] == b'/' {
            suffix = ab.len() - (i - 1);
        }
        i -= 1;
        j -= 1;
    }
    let a_mid = ab.len().saturating_sub(prefix + suffix);
    let b_mid = bb.len().saturating_sub(prefix + suffix);
    let mut out = String::new();
    let wrap = prefix + suffix > 0;
    if wrap {
        out.push_str(&a[..prefix]);
        out.push('{');
    }
    out.push_str(&a[prefix..prefix + a_mid]);
    out.push_str(" => ");
    out.push_str(&b[prefix..prefix + b_mid]);
    if wrap {
        out.push('}');
        out.push_str(&a[a.len() - suffix..]);
    }
    out
}

fn digits(n: u64) -> usize {
    n.checked_ilog10()
        .map_or(1, |d| usize::try_from(d).unwrap_or(0) + 1)
}

fn scale(it: u64, width: u64, max: u64) -> u64 {
    if it == 0 {
        0
    } else {
        1 + it * (width - 1) / max
    }
}

struct Layout {
    number_width: usize,
    name_width: usize,
    graph: u64,
    max_change: u64,
}

fn layout(files: &[File], names: &[String]) -> Layout {
    let max_len = names.iter().map(|n| n.chars().count()).max().unwrap_or(0);
    let (mut number_width, mut bin_width, mut max_change) = (0, 0, 0);
    for f in files {
        if f.binary {
            bin_width = bin_width.max(14 + digits(f.added) + digits(f.deleted));
            number_width = 3;
        } else {
            max_change = max_change.max(f.added + f.deleted);
        }
    }
    let number_width = number_width.max(digits(max_change));
    let width = STAT_WIDTH.max(22 + number_width);
    let max_width = usize::try_from(max_change).unwrap_or(usize::MAX);
    let mut graph_width = if max_width + 4 > bin_width {
        max_width
    } else {
        bin_width - 4
    };
    let mut name_width = max_len;
    if name_width + number_width + 6 + graph_width > width {
        let cap = (width * 3 / 8).saturating_sub(number_width + 6).max(6);
        graph_width = graph_width.min(cap);
        let room = width - number_width - 6 - graph_width;
        if name_width > room {
            name_width = room;
        } else {
            graph_width = width - number_width - 6 - name_width;
        }
    }
    Layout {
        number_width,
        name_width,
        graph: u64::try_from(graph_width).unwrap_or(u64::MAX),
        max_change,
    }
}

pub(crate) fn stat(files: &[File]) -> String {
    let names: Vec<String> = files.iter().map(File::stat_name).collect();
    let Layout {
        number_width,
        name_width,
        graph,
        max_change,
    } = layout(files, &names);
    let mut out = String::new();
    let (mut insertions, mut deletions) = (0, 0);
    for (f, name) in files.iter().zip(&names) {
        let mut shown = name.as_str();
        let mut prefix = "";
        let mut room = name_width;
        let count = name.chars().count();
        if name_width < count {
            prefix = "...";
            room = room.saturating_sub(3);
            let skip = name
                .char_indices()
                .nth(count - room)
                .map_or(name.len(), |(i, _)| i);
            shown = &name[skip..];
            if let Some(slash) = shown.find('/') {
                shown = &shown[slash..];
            }
        }
        let pad = room.saturating_sub(shown.chars().count());
        let _ = write!(out, " {prefix}{shown}{:pad$} | ", "");
        if f.binary {
            let _ = write!(out, "{:>number_width$}", "Bin");
            if f.added == 0 && f.deleted == 0 {
                out.push('\n');
            } else {
                let _ = writeln!(out, " {} -> {} bytes", f.deleted, f.added);
            }
            continue;
        }
        insertions += f.added;
        deletions += f.deleted;
        let total = f.added + f.deleted;
        let _ = write!(out, "{total:>number_width$}");
        if total > 0 {
            out.push(' ');
        }
        let (mut add, mut del) = (f.added, f.deleted);
        if graph <= max_change {
            let mut scaled = scale(total, graph, max_change);
            if scaled < 2 && add > 0 && del > 0 {
                scaled = 2;
            }
            if add < del {
                add = scale(add, graph, max_change);
                del = scaled - add;
            } else {
                del = scale(del, graph, max_change);
                add = scaled - del;
            }
        }
        for (sign, n) in [('+', add), ('-', del)] {
            (0..n).for_each(|_| out.push(sign));
        }
        out.push('\n');
    }
    let _ = write!(
        out,
        " {} file{} changed",
        files.len(),
        if files.len() == 1 { "" } else { "s" }
    );
    if insertions > 0 || deletions == 0 {
        let s = if insertions == 1 { "" } else { "s" };
        let _ = write!(out, ", {insertions} insertion{s}(+)");
    }
    if deletions > 0 || insertions == 0 {
        let s = if deletions == 1 { "" } else { "s" };
        let _ = write!(out, ", {deletions} deletion{s}(-)");
    }
    out.push('\n');
    summary(&mut out, files);
    out
}

fn summary(out: &mut String, files: &[File]) {
    for f in files {
        match (&f.old, &f.new) {
            (None, Some(new)) => {
                let _ = writeln!(
                    out,
                    " create mode {} {}",
                    new.mode.as_str(),
                    quote("", f.path())
                );
            }
            (Some(old), None) => {
                let _ = writeln!(
                    out,
                    " delete mode {} {}",
                    old.mode.as_str(),
                    quote("", f.path())
                );
            }
            _ if f.renamed() => {
                let _ = writeln!(out, " rename {} (100%)", f.stat_name());
                if let Some((a, b)) = f.modes() {
                    let _ = writeln!(out, " mode change {} => {}", a.as_str(), b.as_str());
                }
            }
            _ => {
                if let Some((a, b)) = f.modes() {
                    let _ = writeln!(
                        out,
                        " mode change {} => {} {}",
                        a.as_str(),
                        b.as_str(),
                        quote("", f.path())
                    );
                }
            }
        }
    }
}

pub(crate) fn diff_text(files: &[File], abbrev: usize, binary_patch: bool) -> String {
    let mut out = String::new();
    for f in files {
        f.write(&mut out, abbrev, binary_patch);
    }
    out
}

fn binary_body(out: &mut String, old: &[u8], new: &[u8]) {
    out.push_str("GIT binary patch\n");
    let (forward, reverse) = if old.is_empty() || new.is_empty() {
        (None, None)
    } else {
        libgit2_deltas(old, new).unwrap_or_default()
    };
    side(out, new, forward);
    side(out, old, reverse);
}

fn side(out: &mut String, target: &[u8], delta: Option<Vec<u8>>) {
    let literal = deflated(target);
    match delta.map(|d| (d.len(), deflated(&d))) {
        Some((len, z)) if z.len() < literal.len() => block(out, "delta", len, &z),
        _ => block(out, "literal", target.len(), &literal),
    }
}

type Deltas = (Option<Vec<u8>>, Option<Vec<u8>>);

fn libgit2_deltas(old: &[u8], new: &[u8]) -> Option<Deltas> {
    let odb = git2::Odb::new().ok()?;
    odb.add_new_mempack_backend(1).ok()?;
    let repo = Repository::from_odb(odb).ok()?;
    let a = repo.find_blob(repo.blob(old).ok()?).ok()?;
    let b = repo.find_blob(repo.blob(new).ok()?).ok()?;
    let mut opts = DiffOptions::new();
    opts.show_binary(true).force_binary(true);
    let mut found = None;
    let mut on_binary = |_: DiffDelta<'_>, binary: DiffBinary<'_>| {
        found = Some((delta(&binary.new_file()), delta(&binary.old_file())));
        true
    };
    repo.diff_blobs(
        Some(&a),
        None,
        Some(&b),
        None,
        Some(&mut opts),
        None,
        Some(&mut on_binary),
        None,
        None,
    )
    .ok()?;
    found
}

fn delta(file: &DiffBinaryFile<'_>) -> Option<Vec<u8>> {
    if !matches!(file.kind(), DiffBinaryKind::Delta) {
        return None;
    }
    let mut raw = Vec::with_capacity(file.inflated_len());
    ZlibDecoder::new(file.data()).read_to_end(&mut raw).ok()?;
    Some(raw)
}

fn block(out: &mut String, kind: &str, len: usize, deflated: &[u8]) {
    let _ = writeln!(out, "{kind} {len}");
    for chunk in deflated.chunks(BINARY_LINE) {
        let n = u8::try_from(chunk.len()).unwrap_or(0);
        out.push(char::from(if n <= 26 {
            b'A' + n - 1
        } else {
            b'a' + n - 27
        }));
        for group in chunk.chunks(4) {
            let mut acc = (0..4).fold(0_u32, |acc, i| {
                acc << 8 | u32::from(group.get(i).copied().unwrap_or(0))
            });
            let mut five = [0_u8; 5];
            for d in five.iter_mut().rev() {
                *d = BASE85[usize::try_from(acc % 85).unwrap_or(0)];
                acc /= 85;
            }
            for c in five {
                out.push(char::from(c));
            }
        }
        out.push('\n');
    }
    out.push('\n');
}

fn deflated(data: &[u8]) -> Vec<u8> {
    let mut z = Compress::new(Compression::fast(), true);
    let mut out = Vec::with_capacity(data.len() + data.len() / 8 + 64);
    loop {
        let read = usize::try_from(z.total_in()).unwrap_or(data.len());
        let rest = data.get(read..).unwrap_or_default();
        match z.compress_vec(rest, &mut out, FlushCompress::Finish) {
            Ok(Status::StreamEnd) | Err(_) => return out,
            Ok(Status::Ok | Status::BufError) => out.reserve(out.capacity().max(64)),
        }
    }
}

fn date(who: &Signature) -> String {
    let local = who.time.unix() + i64::from(who.offset_minutes) * 60;
    let days = local.div_euclid(86_400);
    let secs = local.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let weekday = DAYS[usize::try_from(days.rem_euclid(7)).unwrap_or(0)];
    let month = MONTHS[usize::try_from(month - 1).unwrap_or(0)];
    let sign = if who.offset_minutes < 0 { '-' } else { '+' };
    let offset = who.offset_minutes.unsigned_abs();
    format!(
        "{weekday}, {day} {month} {year} {:02}:{:02}:{:02} {sign}{:02}{:02}",
        secs / 3600,
        secs / 60 % 60,
        secs % 60,
        offset / 60,
        offset % 60
    )
}

fn last_line(out: &str) -> usize {
    out.len() - out.rfind('\n').map_or(0, |i| i + 1)
}

fn needs_encoding(text: &str) -> bool {
    !text.is_ascii() || text.contains("=?")
}

fn rfc2047(out: &mut String, text: &str, address: bool) {
    let mut line = last_line(out) + 10;
    out.push_str("=?UTF-8?q?");
    for c in text.chars() {
        let mut buf = [0_u8; 4];
        let bytes = c.encode_utf8(&mut buf).as_bytes();
        let special = bytes.len() > 1
            || !c.is_ascii_graphic()
            || matches!(c, '=' | '?' | '_')
            || (address && !c.is_ascii_alphanumeric() && !matches!(c, '!' | '*' | '+' | '-' | '/'));
        let width = if special { 3 * bytes.len() } else { 1 };
        if line + width + 2 > ENCODED_WIDTH {
            out.push_str("?=\n =?UTF-8?q?");
            line = 11;
        }
        if special {
            for b in bytes {
                let _ = write!(out, "={b:02X}");
            }
        } else {
            out.push(c);
        }
        line += width;
    }
    out.push_str("?=");
}

fn wrap(out: &mut String, text: &str) {
    let bytes = text.as_bytes();
    let first = last_line(out);
    let (mut pos, mut bol, mut w) = (0, 0, first);
    let mut space = Some(0);
    let mut indent = 0;
    loop {
        let c = bytes.get(pos).copied();
        if c.is_none_or(|c| c.is_ascii_whitespace()) {
            if w <= MAIL_WIDTH || space.is_none() {
                if c.is_none() && pos == bol {
                    return;
                }
                let start = space.unwrap_or_else(|| {
                    (0..indent).for_each(|_| out.push(' '));
                    bol
                });
                out.push_str(&text[start..pos]);
                if c.is_none() {
                    return;
                }
                space = Some(pos);
                w += 1;
                pos += 1;
            } else {
                out.push('\n');
                let s = space.unwrap_or(bol);
                pos = s + usize::from(bytes.get(s).is_some_and(u8::is_ascii_whitespace));
                bol = pos;
                space = None;
                w = 1;
                indent = 1;
            }
            continue;
        }
        let step = text[pos..].chars().next().map_or(1, char::len_utf8);
        w += 1;
        pos += step;
    }
}

fn quoted_name(name: &str) -> String {
    if !name.contains([
        '(', ')', '<', '>', '[', ']', ':', ';', '@', ',', '.', '"', '\\',
    ]) {
        return name.to_owned();
    }
    let mut out = String::from("\"");
    for c in name.chars() {
        if matches!(c, '"' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

/// One `git format-patch` mail as GitHub writes it: no signature, and `[PATCH n/m]` when
/// `number` gives the commit's place among `m`.
pub(crate) fn mail(
    id: ObjectId,
    commit: &Commit,
    number: Option<(usize, usize)>,
    files: &[File],
    abbrev: usize,
) -> String {
    let who = &commit.author;
    let mut out = format!("From {id} Mon Sep 17 00:00:00 2001\nFrom: ");
    let mut width = MAIL_WIDTH;
    if needs_encoding(&who.name) {
        rfc2047(&mut out, &who.name, true);
        width = ENCODED_WIDTH;
    } else {
        wrap(&mut out, &quoted_name(&who.name));
    }
    if width < last_line(&out) + 3 + who.email.len() {
        out.push('\n');
    }
    let _ = writeln!(out, " <{}>\nDate: {}", who.email, date(who));
    let message = commit.message.trim_start_matches('\n');
    let mut lines = message.lines();
    let subject: Vec<&str> = lines
        .by_ref()
        .take_while(|l| !l.trim().is_empty())
        .map(str::trim)
        .collect();
    let subject = subject.join(" ");
    match number {
        Some((n, m)) => {
            let _ = write!(out, "Subject: [PATCH {n}/{m}] ");
        }
        None => out.push_str("Subject: [PATCH] "),
    }
    if needs_encoding(&subject) {
        rfc2047(&mut out, &subject, false);
    } else {
        wrap(&mut out, &subject);
    }
    out.push('\n');
    if !message.is_ascii() {
        out.push_str(
            "MIME-Version: 1.0\nContent-Type: text/plain; charset=UTF-8\nContent-Transfer-Encoding: 8bit\n",
        );
    }
    out.push('\n');
    let body: Vec<&str> = lines.skip_while(|l| l.trim().is_empty()).collect();
    let end = body
        .iter()
        .rposition(|l| !l.trim().is_empty())
        .map_or(0, |i| i + 1);
    for line in &body[..end] {
        out.push_str(line);
        out.push('\n');
    }
    out.push_str("---\n");
    out.push_str(&stat(files));
    out.push('\n');
    out.push_str(&diff_text(files, abbrev, true));
    out
}

#[cfg(test)]
mod tests {
    use std::io::Read as _;

    use super::*;

    #[test]
    fn git_picks_the_abbreviation_from_the_object_count() {
        for (objects, want) in [(0, 7), (20, 7), (1 << 14, 8), (5_000_000, 12)] {
            assert_eq!(abbrev(objects), want, "{objects}");
        }
    }

    #[test]
    fn a_rename_shows_the_common_directories_once() {
        for (a, b, want) in [
            (
                "community/Hardhat.gitignore",
                "community/JavaScript/Hardhat.gitignore",
                "community/{ => JavaScript}/Hardhat.gitignore",
            ),
            ("a.txt", "b.txt", "a.txt => b.txt"),
            ("src/old/x.rs", "src/new/x.rs", "src/{old => new}/x.rs"),
        ] {
            assert_eq!(rename_name(a, b), want);
        }
    }

    const LIVE_PNG: &str = include_str!("../testdata/explore-9d47da0-git-png.patch");

    fn base85_lines(lines: &[&str]) -> Vec<u8> {
        let mut out = Vec::new();
        for line in lines {
            let (len, body) = line.split_at(1);
            let len = match len.as_bytes()[0] {
                c @ b'A'..=b'Z' => usize::from(c - b'A' + 1),
                c => usize::from(c - b'a' + 27),
            };
            let mut bytes = Vec::new();
            for group in body.as_bytes().chunks(5) {
                let word = group.iter().fold(0_u32, |acc, c| {
                    let digit = BASE85.iter().position(|d| d == c).unwrap();
                    acc * 85 + u32::try_from(digit).unwrap()
                });
                bytes.extend(word.to_be_bytes());
            }
            out.extend(&bytes[..len]);
        }
        out
    }

    const CODICON_OLD: &[u8] =
        include_bytes!("../testdata/vscode-codicons-45fc3f549ceb-codicon-old.ttf");
    const CODICON_NEW: &[u8] =
        include_bytes!("../testdata/vscode-codicons-45fc3f549ceb-codicon-new.ttf");
    const LIVE_CODICON: &str =
        include_str!("../testdata/vscode-codicons-45fc3f549ceb-codicon.patch");

    #[test]
    fn a_binary_patch_writes_the_literals_live_github_writes() {
        // Given both sides of topics/git/git.png in github/explore 9d47da0, which the live
        // patch carries as literals: zlib 1.2.12 level 1 streams of 9007 and 11761 bytes
        let body = LIVE_PNG.strip_prefix("GIT binary patch\n").unwrap();
        let sides: Vec<Vec<u8>> = body
            .split_terminator("\n\n")
            .map(|block| {
                let lines: Vec<&str> = block.lines().skip(1).collect();
                let mut data = Vec::new();
                ZlibDecoder::new(base85_lines(&lines).as_slice())
                    .read_to_end(&mut data)
                    .unwrap();
                data
            })
            .collect();
        assert_eq!(sides.len(), 2);

        // When hubdouble writes the binary patch of the change
        let mut out = String::new();
        binary_body(&mut out, &sides[1], &sides[0]);

        // Then it writes the live lines byte for byte
        assert_eq!(out, LIVE_PNG);
    }

    #[test]
    fn a_binary_patch_writes_the_deltas_live_github_writes() {
        // Given dist/codicon.ttf in microsoft/vscode-codicons 45fc3f549ceb, a change that
        // live GitHub wrote as delta 638 and delta 642
        // When hubdouble writes the binary patch of the change
        let mut out = String::new();
        binary_body(&mut out, CODICON_OLD, CODICON_NEW);

        // Then it writes the live lines byte for byte
        assert_eq!(out, LIVE_CODICON);
    }

    #[test]
    fn a_long_subject_folds_at_78_columns() {
        let mut out = String::from("Subject: [PATCH] ");
        wrap(&mut out, "word ".repeat(20).trim_end());
        let lines: Vec<&str> = out.lines().collect();
        assert!(lines.iter().all(|l| l.len() <= MAIL_WIDTH), "{out}");
        assert!(lines[1].starts_with(" word"), "{out}");
    }
}
