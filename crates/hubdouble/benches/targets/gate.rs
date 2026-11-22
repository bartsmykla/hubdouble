use std::fmt::Write as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use std::process::ExitCode;

use serde_json::Value;

const BUDGET: f64 = 0.20;

fn read(path: &Path) -> Result<Value, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))
}

fn regression(goal: &str, base: f64, current: f64) -> Option<f64> {
    if base <= 0.0 {
        return None;
    }
    match goal {
        "below" => Some((current - base) / base),
        "above" => Some((base - current) / base),
        _ => None,
    }
}

fn targets(base: &Value, current: &Value, out: &mut String) -> usize {
    let mut failed = 0;
    let rows = |v: &Value| v["rows"].as_array().cloned().unwrap_or_default();
    let (base_rows, current_rows) = (rows(base), rows(current));
    let find =
        |rows: &[Value], measure: &Value| rows.iter().find(|r| r["measure"] == *measure).cloned();
    for row in &base_rows {
        if find(&current_rows, &row["measure"]).is_none() {
            failed += 1;
            let measure = row["measure"].as_str().unwrap_or_default();
            let _ = writeln!(out, "MISSING  {measure}: measured on main, absent now");
        }
    }
    for row in current_rows {
        let measure = row["measure"].as_str().unwrap_or_default();
        let goal = row["goal"].as_str().unwrap_or_default();
        let new = row["value"].as_f64();
        let Some(base_row) = find(&base_rows, &row["measure"]) else {
            if new.is_none() {
                failed += 1;
                let _ = writeln!(out, "MISSING  {measure}: new and not measured");
            } else {
                let _ = writeln!(out, "new      {measure}");
            }
            continue;
        };
        let (Some(old), Some(new)) = (base_row["value"].as_f64(), new) else {
            failed += 1;
            let _ = writeln!(out, "MISSING  {measure}: no value on main or now");
            continue;
        };
        let Some(change) = regression(goal, old, new) else {
            continue;
        };
        let verdict = if change > BUDGET {
            failed += 1;
            "REGRESS"
        } else {
            "ok"
        };
        let _ = writeln!(
            out,
            "{verdict:8} {measure}: {old:.3} -> {new:.3} ({:+.1} %)",
            change * 100.0
        );
    }
    failed
}

fn estimates(dir: &Path, found: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if path.is_dir() {
            if path.file_name().is_some_and(|n| n == "change") {
                let file = path.join("estimates.json");
                if file.is_file() {
                    found.push(file);
                }
            } else {
                estimates(&path, found);
            }
        }
    }
}

fn criterion(dir: &Path, out: &mut String) -> usize {
    let mut found = Vec::new();
    estimates(dir, &mut found);
    found.sort();
    let mut failed = 0;
    for file in found {
        let Ok(value) = read(&file) else { continue };
        let name = file
            .parent()
            .and_then(Path::parent)
            .and_then(|p| p.strip_prefix(dir).ok())
            .map_or_else(String::new, |p| p.display().to_string());
        let mean = &value["mean"];
        let (Some(point), Some(lower)) = (
            mean["point_estimate"].as_f64(),
            mean["confidence_interval"]["lower_bound"].as_f64(),
        ) else {
            continue;
        };
        let verdict = if lower > BUDGET {
            failed += 1;
            "REGRESS"
        } else {
            "ok"
        };
        let _ = writeln!(
            out,
            "{verdict:8} criterion {name}: {:+.1} % (lower bound {:+.1} %)",
            point * 100.0,
            lower * 100.0
        );
    }
    failed
}

pub(crate) fn run(target: &Path, current: &Path, baseline: Option<&Path>) -> ExitCode {
    let (failed, out) = compare(target, current, baseline);
    let _ = std::io::stdout().lock().write_all(out.as_bytes());
    if failed == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

pub(crate) fn compare(target: &Path, current: &Path, baseline: Option<&Path>) -> (usize, String) {
    let mut out = String::new();
    let mut failed = 0;
    let base = baseline.ok_or_else(|| "HUBDOUBLE_BENCH_BASELINE is not set".to_owned());
    match base.and_then(|base| Ok((read(base)?, read(current)?))) {
        Ok((base, current)) => failed += targets(&base, &current, &mut out),
        Err(e) => {
            failed += 1;
            let _ = writeln!(out, "cannot compare the targets with main: {e}");
        }
    }
    failed += criterion(&target.join("criterion"), &mut out);
    let _ = writeln!(
        out,
        "{failed} regressions beyond the {:.0} % budget",
        BUDGET * 100.0
    );
    (failed, out)
}
