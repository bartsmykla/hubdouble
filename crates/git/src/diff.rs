use std::collections::{BTreeMap, HashMap};

use imara_diff::{Algorithm, Diff, Hunk, InternedInput};

use crate::contract::{EntryMode, FileDiff, FileStatus, GitError, ObjectId};
use crate::odb::Odb;

const BINARY_PROBE: usize = 8000;
const CONTEXT: u32 = 3;

type Leaf = (EntryMode, ObjectId);

pub(crate) struct Change {
    pub path: String,
    pub old: Option<Leaf>,
    pub new: Option<Leaf>,
}

pub(crate) fn tree_changes(
    odb: &Odb,
    a: Option<ObjectId>,
    b: Option<ObjectId>,
) -> Result<Vec<Change>, GitError> {
    let mut out = Vec::new();
    walk(odb, a, b, "", &mut out)?;
    out.sort_by(|x, y| x.path.cmp(&y.path));
    Ok(out)
}

fn walk(
    odb: &Odb,
    a: Option<ObjectId>,
    b: Option<ObjectId>,
    prefix: &str,
    out: &mut Vec<Change>,
) -> Result<(), GitError> {
    if a == b {
        return Ok(());
    }
    let mut joined: BTreeMap<&str, (Option<Leaf>, Option<Leaf>)> = BTreeMap::new();
    let ta = a.map(|id| odb.tree(id)).transpose()?;
    let tb = b.map(|id| odb.tree(id)).transpose()?;
    for e in ta.iter().flat_map(|t| t.iter()) {
        joined.entry(&e.name).or_default().0 = Some((e.mode, e.id));
    }
    for e in tb.iter().flat_map(|t| t.iter()) {
        joined.entry(&e.name).or_default().1 = Some((e.mode, e.id));
    }
    for (name, (ea, eb)) in joined {
        let path = if prefix.is_empty() {
            name.to_owned()
        } else {
            format!("{prefix}/{name}")
        };
        let is_tree = |e: &Leaf| e.0 == EntryMode::Tree;
        let (dir_a, dir_b) = (ea.filter(is_tree), eb.filter(is_tree));
        if dir_a.is_some() || dir_b.is_some() {
            walk(odb, dir_a.map(|e| e.1), dir_b.map(|e| e.1), &path, out)?;
        }
        let (leaf_a, leaf_b) = (ea.filter(|e| !is_tree(e)), eb.filter(|e| !is_tree(e)));
        if leaf_a != leaf_b {
            out.push(Change {
                path,
                old: leaf_a,
                new: leaf_b,
            });
        }
    }
    Ok(())
}

pub(crate) fn file_diffs(odb: &Odb, changes: &[Change]) -> Result<Vec<FileDiff>, GitError> {
    let mut removed: HashMap<ObjectId, Vec<usize>> = HashMap::new();
    for (i, c) in changes.iter().enumerate() {
        if let (Some((_, id)), None) = (c.old, c.new) {
            removed.entry(id).or_default().push(i);
        }
    }
    let mut renamed_from: HashMap<usize, usize> = HashMap::new();
    for (i, c) in changes.iter().enumerate() {
        if let (None, Some((_, id))) = (c.old, c.new)
            && let Some(from) = removed
                .get_mut(&id)
                .and_then(|v| (!v.is_empty()).then(|| v.remove(0)))
        {
            renamed_from.insert(i, from);
        }
    }
    let consumed: std::collections::HashSet<usize> = renamed_from.values().copied().collect();
    let mut out = Vec::with_capacity(changes.len());
    for (i, c) in changes.iter().enumerate() {
        if consumed.contains(&i) {
            continue;
        }
        if let Some(from) = renamed_from.get(&i) {
            let (_, id) = c.new.unwrap_or((EntryMode::File, ObjectId::NULL));
            out.push(FileDiff {
                path: c.path.clone(),
                previous_path: Some(changes[*from].path.clone()),
                status: FileStatus::Renamed,
                blob: id,
                additions: 0,
                deletions: 0,
                patch: None,
            });
            continue;
        }
        let status = match (c.old, c.new) {
            (None, _) => FileStatus::Added,
            (_, None) => FileStatus::Removed,
            _ => FileStatus::Modified,
        };
        let blob = c.new.or(c.old).map_or(ObjectId::NULL, |(_, id)| id);
        let (patch, additions, deletions) = patch(odb, c.old, c.new)?;
        out.push(FileDiff {
            path: c.path.clone(),
            previous_path: None,
            status,
            blob,
            additions,
            deletions,
            patch,
        });
    }
    out.sort_by(|x, y| x.path.cmp(&y.path));
    Ok(out)
}

fn content(odb: &Odb, side: Option<Leaf>) -> Result<Option<std::sync::Arc<[u8]>>, GitError> {
    match side {
        Some((EntryMode::Submodule, _)) => Ok(None),
        Some((_, id)) => odb.blob(id).map(Some),
        None => Ok(Some(std::sync::Arc::from([]))),
    }
}

fn text(bytes: &[u8]) -> Option<&str> {
    if bytes[..bytes.len().min(BINARY_PROBE)].contains(&0) {
        return None;
    }
    std::str::from_utf8(bytes).ok()
}

fn patch(
    odb: &Odb,
    old: Option<Leaf>,
    new: Option<Leaf>,
) -> Result<(Option<String>, u64, u64), GitError> {
    let (Some(a), Some(b)) = (content(odb, old)?, content(odb, new)?) else {
        return Ok((None, 0, 0));
    };
    let (Some(a), Some(b)) = (text(&a), text(&b)) else {
        return Ok((None, 0, 0));
    };
    Ok(text_patch(a, b))
}

fn range(start: u32, len: u32) -> String {
    match len {
        0 => format!("{start},0"),
        1 => format!("{}", start + 1),
        _ => format!("{},{len}", start + 1),
    }
}

fn line(out: &mut String, sign: char, text: &str) {
    out.push(sign);
    out.push_str(text);
    if !text.ends_with('\n') {
        out.push_str("\n\\ No newline at end of file\n");
    }
}

pub(crate) fn text_patch(a: &str, b: &str) -> (Option<String>, u64, u64) {
    let input = InternedInput::new(a, b);
    let mut diff = Diff::compute(Algorithm::Histogram, &input);
    diff.postprocess_lines(&input);
    let hunks: Vec<Hunk> = diff.hunks().collect();
    if hunks.is_empty() {
        return (None, 0, 0);
    }
    let old = |i: u32| input.interner[input.before[i as usize]];
    let new = |i: u32| input.interner[input.after[i as usize]];
    let old_len = u32::try_from(input.before.len()).unwrap_or(u32::MAX);
    let mut out = String::new();
    let mut i = 0;
    while i < hunks.len() {
        let mut j = i;
        while j + 1 < hunks.len() && hunks[j + 1].before.start - hunks[j].before.end <= 2 * CONTEXT
        {
            j += 1;
        }
        let (first, last) = (&hunks[i], &hunks[j]);
        let prev_end = if i == 0 { 0 } else { hunks[i - 1].before.end };
        let next_start = hunks.get(j + 1).map_or(old_len, |h| h.before.start);
        let pre = CONTEXT.min(first.before.start - prev_end);
        let post = CONTEXT.min(next_start - last.before.end);
        let (old_start, old_end) = (first.before.start - pre, last.before.end + post);
        let (new_start, new_end) = (first.after.start - pre, last.after.end + post);
        out.push_str("@@ -");
        out.push_str(&range(old_start, old_end - old_start));
        out.push_str(" +");
        out.push_str(&range(new_start, new_end - new_start));
        out.push_str(" @@\n");
        let mut pos = old_start;
        for h in &hunks[i..=j] {
            (pos..h.before.start).for_each(|k| line(&mut out, ' ', old(k)));
            h.before.clone().for_each(|k| line(&mut out, '-', old(k)));
            h.after.clone().for_each(|k| line(&mut out, '+', new(k)));
            pos = h.before.end;
        }
        (pos..old_end).for_each(|k| line(&mut out, ' ', old(k)));
        i = j + 1;
    }
    out.pop();
    (
        Some(out),
        u64::from(diff.count_additions()),
        u64::from(diff.count_removals()),
    )
}
