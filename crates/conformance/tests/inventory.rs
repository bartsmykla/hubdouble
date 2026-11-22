use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;

use hubdouble_testkit::coverage;

const GATE: &str = "every_inventory_route_gets_a_checked_success_answer";

fn inventory() -> BTreeSet<String> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/inventory/rest-routes.txt");
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(str::to_owned)
        .collect()
}

#[test]
fn every_inventory_route_gets_a_checked_success_answer() {
    // Given every other conformance test, run in its own nextest process, records its checked answers
    let dir = tempfile::TempDir::new().unwrap();
    let mut run = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
    run.args([
        "nextest",
        "run",
        "--workspace",
        "--locked",
        "--no-fail-fast",
        "-E",
        &format!("package(hubdouble-conformance) and not test(={GATE})"),
    ])
    .env(coverage::DIR_ENV, dir.path());
    for (name, _) in std::env::vars_os() {
        if name.to_string_lossy().starts_with("NEXTEST") {
            run.env_remove(name);
        }
    }

    // When the suite runs
    let out = run.output().unwrap();

    // Then it passes and every inventory route has at least one checked 2xx or 3xx answer
    let stderr = String::from_utf8_lossy(&out.stderr);
    let tail: Vec<&str> = stderr.lines().rev().take(40).collect();
    assert!(
        out.status.success(),
        "the conformance suite failed:\n{}",
        tail.into_iter().rev().collect::<Vec<_>>().join("\n")
    );
    let succeeded: BTreeSet<String> = coverage::read(dir.path())
        .into_iter()
        .filter(|c| (200..400).contains(&c.status))
        .map(|c| c.route)
        .collect();
    let missing: Vec<String> = inventory().difference(&succeeded).cloned().collect();
    assert!(
        missing.is_empty(),
        "inventory routes with no checked success answer:\n  {}",
        missing.join("\n  ")
    );
}
