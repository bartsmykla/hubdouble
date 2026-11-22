//! The record of checked answers that the inventory gate reads. Each nextest test runs in its
//! own process, so a run that wants the record sets [`DIR_ENV`] to a `tempfile::TempDir` it
//! owns; every test process appends one line per checked answer to its own file there. Without
//! the variable nothing is written.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use crate::answer::Checked;

/// The directory the checked answers are written to.
pub const DIR_ENV: &str = "HUBDOUBLE_TEST_COVERAGE_DIR";

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

    pub(crate) fn record(&self, checked: &Checked) {
        let Some(file) = &self.file else { return };
        let line = format!("{}\t{}\n", checked.route, checked.status);
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
/// When `dir` or a file in it cannot be read, or a line is not `<route>\t<status>`.
#[must_use]
pub fn read(dir: &Path) -> Vec<Checked> {
    let entries = std::fs::read_dir(dir).unwrap_or_else(|e| panic!("read {}: {e}", dir.display()));
    let mut out = Vec::new();
    for entry in entries {
        let path = entry
            .unwrap_or_else(|e| panic!("read {}: {e}", dir.display()))
            .path();
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        for line in text.lines() {
            let (route, status) = line
                .rsplit_once('\t')
                .unwrap_or_else(|| panic!("a bad line in {}: {line}", path.display()));
            out.push(Checked {
                route: route.to_owned(),
                status: status
                    .parse()
                    .unwrap_or_else(|e| panic!("a bad status in {}: {e}", path.display())),
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
            status: 200,
        };
        // When
        on.record(&checked);
        on.record(&checked);
        off.record(&checked);
        // Then
        assert_eq!(read(dir.path()), vec![checked.clone(), checked]);
    }
}
