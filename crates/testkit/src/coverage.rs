//! The record of checked answers that the inventory gate reads. Each nextest test runs in its
//! own process, so a run that wants the record sets [`DIR_ENV`] to a `tempfile::TempDir` it
//! owns; every test process appends one line per checked answer to its own file there. Without
//! the variable nothing is written.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use crate::answer::Checked;

/// The directory the checked answers are written to.
pub const DIR_ENV: &str = "HUBDOUBLE_TEST_COVERAGE_DIR";

/// Which case of plan section 7's "Tests per operation" a checked answer proves, read from the
/// answer and the server's state, never from what the test says it meant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Case {
    /// A `2xx` or `3xx` answer.
    Success,
    /// A `404` to a request whose `{owner}`/`{repo}` or `{repository_id}` names a repository
    /// that does not exist on the server, so no actor can see it.
    UnseenRepository,
    /// A `403` with `X-Accepted-GitHub-Permissions`, GitHub's answer to a token without the
    /// operation's permission.
    MissingPermission,
    /// A `422`.
    Invalid,
    /// Any other answer.
    Other,
}

impl Case {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Case::Success => "success",
            Case::UnseenRepository => "unseen-repository",
            Case::MissingPermission => "missing-permission",
            Case::Invalid => "invalid",
            Case::Other => "other",
        }
    }

    fn parse(s: &str) -> Option<Case> {
        [
            Case::Success,
            Case::UnseenRepository,
            Case::MissingPermission,
            Case::Invalid,
            Case::Other,
        ]
        .into_iter()
        .find(|c| c.as_str() == s)
    }
}

/// One recorded answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    /// The route as `docs/inventory/rest-routes.txt` writes it.
    pub route: String,
    pub status: u16,
    pub case: Case,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct Recorder {
    file: Option<PathBuf>,
}

impl Recorder {
    pub(crate) fn new(lookup: &dyn Fn(&str) -> Option<String>) -> Recorder {
        Recorder {
            file: lookup(DIR_ENV)
                .filter(|d| !d.is_empty())
                .map(|d| PathBuf::from(d).join(format!("{}.tsv", std::process::id()))),
        }
    }

    pub(crate) fn from_process_env() -> Recorder {
        Recorder::new(&|name| std::env::var(name).ok())
    }

    pub(crate) fn record(&self, checked: &Checked, case: Case) {
        let Some(file) = &self.file else { return };
        let line = format!("{}\t{}\t{}\n", checked.route, checked.status, case.as_str());
        let written = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(file)
            .and_then(|mut f| f.write_all(line.as_bytes()));
        if let Err(e) = written {
            panic!("record a checked answer in {}: {e}", file.display());
        }
    }
}

/// Every checked answer recorded under `dir`.
///
/// # Panics
///
/// When `dir` or a file in it cannot be read, or a line is not `<route>\t<status>\t<case>`.
#[must_use]
pub fn read(dir: &Path) -> Vec<Record> {
    let entries = std::fs::read_dir(dir).unwrap_or_else(|e| panic!("read {}: {e}", dir.display()));
    let mut out = Vec::new();
    for entry in entries {
        let path = entry
            .unwrap_or_else(|e| panic!("read {}: {e}", dir.display()))
            .path();
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        for line in text.lines() {
            let bad = || panic!("a bad line in {}: {line}", path.display());
            let mut fields = line.rsplitn(3, '\t');
            let (Some(case), Some(status), Some(route)) =
                (fields.next(), fields.next(), fields.next())
            else {
                bad()
            };
            out.push(Record {
                route: route.to_owned(),
                status: status.parse().unwrap_or_else(|_| bad()),
                case: Case::parse(case).unwrap_or_else(|| bad()),
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_go_to_the_directory_only_when_it_is_set() {
        // Given
        let dir = tempfile::TempDir::new().unwrap();
        let set = dir.path().to_string_lossy().into_owned();
        let on = Recorder::new(&|n| (n == DIR_ENV).then(|| set.clone()));
        let off = Recorder::new(&|_| None);
        let checked = Checked {
            route: "GET /repos/{owner}/{repo}".to_owned(),
            status: 404,
        };
        // When
        on.record(&checked, Case::UnseenRepository);
        on.record(&checked, Case::Other);
        off.record(&checked, Case::Other);
        // Then
        let record = |case| Record {
            route: checked.route.clone(),
            status: 404,
            case,
        };
        assert_eq!(
            read(dir.path()),
            vec![record(Case::UnseenRepository), record(Case::Other)]
        );
    }
}
