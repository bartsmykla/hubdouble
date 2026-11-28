use std::collections::BTreeSet;
use std::error::Error;
use std::path::Path;
use std::process::Command;

use crate::COVERAGE;

pub fn run(root: &Path, args: &[String]) -> Result<(), Box<dyn Error>> {
    let [base] = args else {
        return Err("usage: xtask coverage-ratchet <base revision>".into());
    };
    let shown = Command::new("git")
        .current_dir(root)
        .args(["show", &format!("{base}:{COVERAGE}")])
        .output()?;
    if !shown.status.success() {
        eprintln!("xtask: {base} has no {COVERAGE}, so nothing can go missing");
        return Ok(());
    }
    let head = std::fs::read_to_string(root.join(COVERAGE))?;
    let lost = lost(&String::from_utf8(shown.stdout)?, &head);
    if lost.is_empty() {
        eprintln!("xtask: every operation registered on {base} is still registered");
        Ok(())
    } else {
        Err(format!(
            "operations registered on {base} but missing now:\n  {}",
            lost.join("\n  ")
        )
        .into())
    }
}

fn missing(report: &str) -> BTreeSet<&str> {
    report
        .split_once("\nmissing:\n")
        .map_or("", |(_, list)| list)
        .lines()
        .filter_map(|line| line.strip_prefix("    "))
        .collect()
}

fn lost<'h>(base: &str, head: &'h str) -> Vec<&'h str> {
    let before = missing(base);
    missing(head)
        .into_iter()
        .filter(|id| !before.contains(id))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = "operations: 1 of 3 registered (33.3%)\nextra routes: 0\nmissing:\n  meta:\n    meta/get\n    meta/root\n";

    #[test]
    fn an_operation_registered_on_the_base_and_missing_on_the_head_is_lost() {
        // Given a head that registers meta/get but drops emojis/get, which the base registered
        let head = "operations: 1 of 3 registered (33.3%)\nextra routes: 0\nmissing:\n  emojis:\n    emojis/get\n  meta:\n    meta/root\n";

        // When the two reports are compared
        let lost = lost(BASE, head);

        // Then only the dropped operation fails the ratchet, though the count stayed the same
        assert_eq!(lost, ["emojis/get"]);
    }

    #[test]
    fn a_head_that_registers_more_loses_nothing() {
        // Given a head that registers meta/get too
        let head = "operations: 2 of 3 registered (66.7%)\nextra routes: 0\nmissing:\n  meta:\n    meta/root\n";

        // When the two reports are compared
        let lost = lost(BASE, head);

        // Then nothing is lost
        assert_eq!(lost, [] as [&str; 0]);
    }
}
