use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use std::process::ExitCode;

use serde_json::Value;

use crate::row::{Goal, Row, Scale, number};

const BUDGET: f64 = 0.20;

struct Round {
    number: u32,
    base: Vec<Row>,
    head: Vec<Row>,
}

fn read(path: &Path) -> Result<Value, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))
}

fn rows(path: &Path) -> Result<Vec<Row>, String> {
    read(path)?["rows"]
        .as_array()
        .and_then(|r| r.iter().map(Row::from_json).collect::<Option<Vec<_>>>())
        .ok_or_else(|| format!("{}: unreadable rows", path.display()))
}

fn rounds(dir: &Path) -> Result<Vec<Round>, String> {
    let entries = std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let mut sides: BTreeMap<u32, (Option<PathBuf>, Option<PathBuf>)> = BTreeMap::new();
    for path in entries.filter_map(Result::ok).map(|e| e.path()) {
        let Some(stem) = path
            .extension()
            .is_some_and(|e| e == "json")
            .then(|| path.file_stem().and_then(|s| s.to_str()))
            .flatten()
        else {
            continue;
        };
        let Some((side @ ("base" | "head"), n)) = stem.split_once('-') else {
            continue;
        };
        let n: u32 = n
            .parse()
            .map_err(|_| format!("{}: not a round number", path.display()))?;
        let slot = sides.entry(n).or_default();
        if side == "base" {
            slot.0 = Some(path);
        } else {
            slot.1 = Some(path);
        }
    }
    if sides.is_empty() {
        return Err(format!("{}: no base-N.json or head-N.json", dir.display()));
    }
    sides
        .into_iter()
        .map(|(number, pair)| match pair {
            (Some(base), Some(head)) => Ok(Round {
                number,
                base: rows(&base)?,
                head: rows(&head)?,
            }),
            (None, _) => Err(format!("round {number} has no base report")),
            (_, None) => Err(format!("round {number} has no head report")),
        })
        .collect()
}

fn median(mut values: Vec<f64>) -> Option<f64> {
    values.sort_by(f64::total_cmp);
    let n = values.len();
    match n {
        0 => None,
        _ if n % 2 == 1 => Some(values[n / 2]),
        _ => Some(f64::midpoint(values[n / 2 - 1], values[n / 2])),
    }
}

fn find<'a>(rows: &'a [Row], measure: &str) -> Option<&'a Row> {
    rows.iter().find(|r| r.measure == measure)
}

fn regression(goal: Goal, base: f64, current: f64) -> Option<f64> {
    if base <= 0.0 {
        return None;
    }
    match goal {
        Goal::Below(_) => Some((current - base) / base),
        Goal::Above(_) => Some((base - current) / base),
        Goal::Report => None,
    }
}

fn relative(rounds: &[Round], out: &mut String) -> usize {
    let mut failed = 0;
    let first = &rounds[0];
    for row in &first.base {
        if find(&first.head, &row.measure).is_none() {
            failed += 1;
            let _ = writeln!(
                out,
                "MISSING  {}: measured on main, absent now",
                row.measure
            );
        }
    }
    for row in &first.head {
        let measure = row.measure.as_str();
        if rounds.iter().all(|r| find(&r.base, measure).is_none()) {
            if row.value.is_none() {
                failed += 1;
                let _ = writeln!(out, "MISSING  {measure}: new and not measured");
            } else {
                let _ = writeln!(out, "new      {measure}");
            }
            continue;
        }
        let mut pairs = Vec::new();
        for round in rounds {
            let value = |rows: &[Row]| find(rows, measure).and_then(|r| r.value);
            if let (Some(old), Some(new)) = (value(&round.base), value(&round.head)) {
                pairs.push((old, new));
            } else {
                failed += 1;
                let _ = writeln!(
                    out,
                    "MISSING  {measure}: no value on main or now in round {}",
                    round.number
                );
            }
        }
        if pairs.len() < rounds.len() {
            continue;
        }
        let changes: Vec<f64> = pairs
            .iter()
            .filter_map(|&(old, new)| regression(row.goal, old, new))
            .collect();
        let (Some(change), Some(old), Some(new)) = (
            median(changes),
            median(pairs.iter().map(|p| p.0).collect()),
            median(pairs.iter().map(|p| p.1).collect()),
        ) else {
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
            "{verdict:8} {measure}: {old:.3} -> {new:.3} ({:+.1} %, median of {} paired rounds)",
            change * 100.0,
            pairs.len()
        );
    }
    failed
}

fn factors(round: &Round) -> BTreeMap<&'static str, Option<f64>> {
    Scale::ALL
        .into_iter()
        .map(|s| {
            let factor = find(&round.head, s.measure())
                .and_then(|r| r.value)
                .filter(|v| *v > 0.0)
                .map(|v| v / s.reference());
            (s.name(), factor)
        })
        .collect()
}

fn absolute(rounds: &[Round], out: &mut String) -> usize {
    let mut failed = 0;
    let factors: Vec<_> = rounds.iter().map(factors).collect();
    for scale in Scale::ALL {
        let seen: Vec<f64> = factors.iter().filter_map(|f| f[scale.name()]).collect();
        let runs: Vec<String> = seen.iter().map(|f| format!("{f:.2}")).collect();
        let _ = writeln!(
            out,
            "runner   {}: factor {} against the reference {} µs (rounds {})",
            scale.measure(),
            median(seen.clone()).map_or_else(|| "n/a".to_owned(), |f| format!("{f:.2}")),
            number(scale.reference()),
            runs.join(", ")
        );
    }
    for row in &rounds[0].head {
        if matches!(row.goal, Goal::Report) {
            continue;
        }
        let mut raw = Vec::new();
        let mut judged = Vec::new();
        for (round, factor) in rounds.iter().zip(&factors) {
            let Some(value) = find(&round.head, &row.measure).and_then(|r| r.value) else {
                continue;
            };
            let scaled = match row.scale {
                None => Some(value),
                Some(s) => factors_value(factor, s).map(|f| match row.goal {
                    Goal::Above(_) => value * f,
                    _ => value / f,
                }),
            };
            raw.push(value);
            judged.extend(scaled);
        }
        let complete = judged.len() == rounds.len();
        let value = median(judged).filter(|_| complete);
        let met = row.met_by(value);
        if !met {
            failed += 1;
        }
        let shown = value.map_or_else(
            || "n/a".to_owned(),
            |v| format!("{} {}", number(v), row.unit),
        );
        let here = match (row.scale, median(raw)) {
            (Some(_), Some(r)) => {
                format!(" on the reference runner, {} {} here", number(r), row.unit)
            }
            _ => String::new(),
        };
        let _ = writeln!(
            out,
            "{:8} {}: {shown}{here}, target {}",
            if met { "met" } else { "MISS" },
            row.measure,
            row.target()
        );
    }
    failed
}

fn factors_value(factors: &BTreeMap<&'static str, Option<f64>>, scale: Scale) -> Option<f64> {
    factors.get(scale.name()).copied().flatten()
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

pub(crate) fn run(target: &Path, rounds: Option<&Path>) -> ExitCode {
    let (failed, out) = compare(target, rounds);
    let _ = std::io::stdout().lock().write_all(out.as_bytes());
    if failed == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

pub(crate) fn compare(target: &Path, dir: Option<&Path>) -> (usize, String) {
    let mut out = String::new();
    let mut failed = 0;
    let measured = dir
        .ok_or_else(|| "HUBDOUBLE_BENCH_ROUNDS is not set".to_owned())
        .and_then(rounds);
    match measured {
        Ok(rounds) => {
            let _ = writeln!(
                out,
                "head against main, {} interleaved rounds each:",
                rounds.len()
            );
            failed += relative(&rounds, &mut out);
            let _ = writeln!(out, "plan section 9 targets, median of the head rounds:");
            failed += absolute(&rounds, &mut out);
        }
        Err(e) => {
            failed += 1;
            let _ = writeln!(out, "cannot compare the targets with main: {e}");
        }
    }
    failed += criterion(&target.join("criterion"), &mut out);
    let _ = writeln!(
        out,
        "{failed} regressions beyond the {:.0} % budget or missed targets",
        BUDGET * 100.0
    );
    (failed, out)
}
