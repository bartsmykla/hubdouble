use hubdouble_git::{FileDiff, FileStatus, Git, ObjectId, RefChange};
use hubdouble_model::{ApiError, Issue, IssueState, ReviewThread, Side, SubjectType};
use hubdouble_store::{Issues, ReviewComments, ReviewThreads, Tx, TxMut};
use serde::Deserialize;

use crate::{invalid, view::pull_fields};

#[derive(Default, Deserialize)]
pub struct Anchor {
    pub path: String,
    pub line: Option<i64>,
    pub start_line: Option<i64>,
    pub side: Option<Side>,
    pub start_side: Option<Side>,
    pub position: Option<i64>,
    pub subject_type: Option<SubjectType>,
}

pub struct Placement {
    pub path: String,
    pub line: Option<i64>,
    pub start_line: Option<i64>,
    pub side: Side,
    pub start_side: Option<Side>,
    pub position: Option<i64>,
    pub subject_type: SubjectType,
    pub hunk: String,
}

fn bad(field: &str) -> ApiError {
    invalid(
        "PullRequestReviewComment",
        field,
        "The comment must target the pull request diff",
    )
}

pub fn commit(
    tx: &Tx,
    git: &Git,
    issue: &Issue,
    requested: Option<&str>,
) -> Result<ObjectId, ApiError> {
    let pull = pull_fields(issue)?;
    let sha = requested.unwrap_or(&pull.head.sha);
    let id = sha.parse().map_err(|_| bad("commit_id"))?;
    let head_repo = pull.head.repo_id.unwrap_or(issue.repo_id);
    if git.peel_to_commit(tx, head_repo, id).ok() != Some(id) {
        return Err(bad("commit_id"));
    }
    Ok(id)
}

pub fn files(tx: &Tx, git: &Git, issue: &Issue, head: ObjectId) -> Result<Vec<FileDiff>, ApiError> {
    let pull = pull_fields(issue)?;
    let base = pull.base.sha.parse().map_err(|_| bad("commit_id"))?;
    Ok(git
        .compare(tx, pull.head.repo_id.unwrap_or(issue.repo_id), base, head)?
        .files)
}

fn range_start(text: &str) -> Option<i64> {
    text.get(1..)?.split(',').next()?.parse().ok()
}

#[derive(Clone, Copy)]
struct DiffLine {
    side: Side,
    line: i64,
    position: i64,
}

fn lines(patch: &str) -> impl Iterator<Item = DiffLine> {
    let (mut left, mut right, mut position) = (0, 0, -1);
    patch.lines().flat_map(move |text| {
        position += 1;
        if let Some(range) = text.strip_prefix("@@ ") {
            let mut parts = range.split_ascii_whitespace();
            left = parts.next().and_then(range_start).unwrap_or_default();
            right = parts.next().and_then(range_start).unwrap_or_default();
            return [None, None].into_iter().flatten();
        }
        let old = DiffLine {
            side: Side::Left,
            line: left,
            position,
        };
        let new = DiffLine {
            side: Side::Right,
            line: right,
            position,
        };
        let pair = match text.as_bytes().first() {
            Some(b'-') => {
                left += 1;
                [Some(old), None]
            }
            Some(b'+') => {
                right += 1;
                [None, Some(new)]
            }
            Some(b' ') => {
                left += 1;
                right += 1;
                [Some(old), Some(new)]
            }
            _ => [None, None],
        };
        pair.into_iter().flatten()
    })
}

impl Anchor {
    pub fn locate(self, files: &[FileDiff]) -> Result<Placement, ApiError> {
        let file = files
            .iter()
            .find(|file| file.path == self.path)
            .ok_or_else(|| bad("path"))?;
        let patch = file.patch.as_deref().unwrap_or_default();
        let subject_type = self.subject_type.unwrap_or(SubjectType::Line);
        let mut placement = Placement {
            path: self.path,
            line: self.line,
            start_line: self.start_line,
            side: self.side.unwrap_or(Side::Right),
            start_side: self.start_side,
            position: self.position,
            subject_type,
            hunk: patch.to_owned(),
        };
        if subject_type == SubjectType::File {
            if placement.line.is_some()
                || placement.start_line.is_some()
                || placement.position.is_some()
            {
                return Err(bad("subject_type"));
            }
            return Ok(placement);
        }
        if let Some(position) = self.position {
            let found = lines(patch)
                .filter(|line| line.position == position)
                .last()
                .ok_or_else(|| bad("position"))?;
            placement.line = Some(found.line);
            placement.side = found.side;
        } else {
            let line = self.line.ok_or_else(|| bad("line"))?;
            let found = lines(patch)
                .find(|entry| entry.line == line && entry.side == placement.side)
                .ok_or_else(|| bad("line"))?;
            placement.position = Some(found.position);
        }
        if let Some(start) = self.start_line {
            let side = self.start_side.unwrap_or(placement.side);
            if placement.line.is_some_and(|line| start >= line)
                || !lines(patch).any(|entry| entry.line == start && entry.side == side)
            {
                return Err(bad("start_line"));
            }
            placement.start_side = Some(side);
        }
        Ok(placement)
    }
}

pub fn ref_changed(tx: &mut TxMut<'_>, git: &Git, change: &RefChange) -> Result<(), ApiError> {
    let Some(old) = change.old else {
        return Ok(());
    };
    let Some(new) = change.new else {
        return Ok(());
    };
    let Some(branch) = change.name.strip_prefix("refs/heads/") else {
        return Ok(());
    };
    let old_sha = old.to_string();
    let new_sha = new.to_string();
    let threads: Vec<_> = tx
        .table::<ReviewThreads>()
        .iter()
        .filter(|thread| {
            !thread.is_outdated
                && thread.subject_type == SubjectType::Line
                && thread.diff_side == Side::Right
        })
        .filter(|thread| {
            let issue = tx
                .table::<Issues>()
                .get(thread.issue_id)
                .filter(|i| i.state == IssueState::Open);
            issue.and_then(|i| i.pull.as_ref()).is_some_and(|p| {
                p.head.repo_id == Some(change.repo_id)
                    && p.head.ref_name == branch
                    && (p.head.sha == old_sha || p.head.sha == new_sha)
            })
        })
        .cloned()
        .collect();
    if threads.is_empty() {
        return Ok(());
    }
    let changed = git.compare(tx, change.repo_id, old, new)?.files;
    for thread in threads {
        if let Some(file) = changed.iter().find(|file| {
            file.path == thread.path || file.previous_path.as_ref() == Some(&thread.path)
        }) {
            update_thread(tx, &thread, &new_sha, file, change.forced)?;
        }
    }
    Ok(())
}

fn mapped_line(patch: &str, line: i64) -> Option<i64> {
    let (mut old, mut new) = (1, 1);
    for row in patch.lines() {
        if let Some(header) = row.strip_prefix("@@ ") {
            let mut ranges = header.split_ascii_whitespace();
            let old_start = ranges.next().and_then(range_start)?;
            if line < old_start {
                return Some(line + new - old);
            }
            old = old_start;
            new = ranges.next().and_then(range_start)?;
            continue;
        }
        match row.as_bytes().first() {
            Some(b'-') if old == line => return None,
            Some(b'-') => old += 1,
            Some(b'+') => new += 1,
            Some(b' ') if old == line => return Some(new),
            Some(b' ') => {
                old += 1;
                new += 1;
            }
            _ => {}
        }
    }
    Some(line + new - old)
}

fn update_thread(
    tx: &mut TxMut<'_>,
    thread: &ReviewThread,
    sha: &str,
    file: &FileDiff,
    forced: bool,
) -> Result<(), ApiError> {
    let patch = file.patch.as_deref();
    let map = |line: i64| patch.map_or(Some(line), |patch| mapped_line(patch, line));
    let mut updated = thread.clone();
    updated.path.clone_from(&file.path);
    if forced || file.status == FileStatus::Removed {
        updated.line = None;
    } else {
        updated.line = thread.line.and_then(map);
        if thread.start_diff_side == Some(Side::Right) {
            updated.start_line = thread.start_line.and_then(map);
        }
    }
    updated.is_outdated =
        updated.line.is_none() || (updated.start_line.is_none() && thread.start_line.is_some());
    if updated.is_outdated {
        updated.line = None;
        updated.start_line = None;
    }
    if updated != *thread {
        tx.table_mut::<ReviewThreads>()
            .replace(updated.clone())
            .map_err(|e| ApiError::internal(e.to_string()))?;
    }
    let comments: Vec<_> = tx
        .table::<ReviewComments>()
        .children(thread.issue_id)
        .filter(|c| c.thread_id == thread.id)
        .cloned()
        .collect();
    for comment in comments {
        let mut comment = (*comment).clone();
        comment.path.clone_from(&updated.path);
        comment.line = updated.line;
        comment.start_line = updated.start_line;
        comment.position = None;
        sha.clone_into(&mut comment.commit_id);
        tx.table_mut::<ReviewComments>()
            .replace(comment)
            .map_err(|e| ApiError::internal(e.to_string()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::mapped_line;

    #[test]
    fn maps_unchanged_lines_and_removes_changed_lines() {
        let patch = "@@ -2,3 +2,4 @@\n before\n-old\n+new\n+added\n after";
        assert_eq!(mapped_line(patch, 1), Some(1));
        assert_eq!(mapped_line(patch, 3), None);
        assert_eq!(mapped_line(patch, 4), Some(5));
        assert_eq!(mapped_line(patch, 20), Some(21));
    }
}
