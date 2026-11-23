use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use std::process::ExitCode;

use serde_json::Value;

use crate::row::{CALIBRATION, Goal, REFERENCE_CALIBRATION_US, REFERENCE_CPU, Row, number};

const BUDGET: f64 = 0.20;
const ROUNDS: usize = 5;

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

fn factor(rows: &[Row]) -> Option<f64> {
    find(rows, CALIBRATION)
        .and_then(|r| r.value)
        .filter(|v| *v > 0.0)
        .map(|v| v / REFERENCE_CALIBRATION_US)
}

struct Judged {
    value: Option<f64>,
    raw: Option<f64>,
}

fn judge(rounds: &[Round], row: &Row, side: fn(&Round) -> &[Row]) -> Judged {
    let mut raw = Vec::new();
    let mut judged = Vec::new();
    for round in rounds {
        let Some(value) = find(side(round), &row.measure).and_then(|r| r.value) else {
            continue;
        };
        let scaled = if row.scaled {
            factor(side(round))
                .or_else(|| factor(&round.head))
                .map(|f| match row.goal {
                    Goal::Above(_) => value * f,
                    _ => value / f,
                })
        } else {
            Some(value)
        };
        raw.push(value);
        judged.extend(scaled);
    }
    let complete = judged.len() == rounds.len();
    Judged {
        value: median(judged).filter(|_| complete),
        raw: median(raw),
    }
}

fn shown(row: &Row, judged: &Judged) -> String {
    let value = judged.value.map_or_else(
        || "n/a".to_owned(),
        |v| format!("{} {}", number(v), row.unit),
    );
    match judged.raw {
        Some(r) if row.scaled => format!(
            "{value} on the reference runner, {} {} here",
            number(r),
            row.unit
        ),
        _ => value,
    }
}

fn is_reference(cpu: Option<&str>) -> bool {
    cpu.is_none_or(|c| c.trim().eq_ignore_ascii_case(REFERENCE_CPU))
}

fn absolute(rounds: &[Round], cpu: Option<&str>, out: &mut String) -> usize {
    let mut failed = 0;
    let seen: Vec<f64> = rounds.iter().filter_map(|r| factor(&r.head)).collect();
    let runs: Vec<String> = seen.iter().map(|f| format!("{f:.2}")).collect();
    let reference = is_reference(cpu);
    let _ = writeln!(
        out,
        "runner   {}{}: {CALIBRATION} factor {} against the reference {} µs (rounds {})",
        cpu.map_or("CPU not given", str::trim),
        if reference {
            ", judged as the reference model"
        } else {
            ", not the reference model"
        },
        median(seen.clone()).map_or_else(|| "n/a".to_owned(), |f| format!("{f:.2}")),
        number(REFERENCE_CALIBRATION_US),
        runs.join(", ")
    );
    for row in &rounds[0].head {
        if matches!(row.goal, Goal::Report) {
            continue;
        }
        let head = judge(rounds, row, |r| &r.head);
        let line = if row.met_by(head.value) {
            format!("met      {}: {}", row.measure, shown(row, &head))
        } else if reference || !row.scaled || head.value.is_none() {
            failed += 1;
            format!("MISS     {}: {}", row.measure, shown(row, &head))
        } else {
            let base = judge(rounds, row, |r| &r.base);
            let tag = if base.value.is_some() && !row.met_by(base.value) {
                "RUNNER"
            } else {
                failed += 1;
                "MISS"
            };
            format!(
                "{tag:8} {}: {}, main {}",
                row.measure,
                shown(row, &head),
                shown(row, &base)
            )
        };
        let _ = writeln!(out, "{line}, target {}", row.target());
    }
    failed
}

type Baselines = BTreeMap<String, BTreeMap<(&'static str, u32), f64>>;

fn baselines(root: &Path, dir: &Path, found: &mut Baselines) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for path in entries.filter_map(Result::ok).map(|e| e.path()) {
        if !path.is_dir() {
            continue;
        }
        let round = path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.split_once('-'))
            .and_then(|(side, n)| {
                let side = ["base", "head"].into_iter().find(|s| *s == side)?;
                Some((side, n.parse::<u32>().ok()?))
            });
        let Some(round) = round else {
            baselines(root, &path, found);
            continue;
        };
        let median = read(&path.join("estimates.json"))
            .ok()
            .and_then(|v| v["median"]["point_estimate"].as_f64());
        let name = dir
            .strip_prefix(root)
            .map_or_else(|_| String::new(), |p| p.display().to_string());
        if let Some(median) = median {
            found.entry(name).or_default().insert(round, median);
        }
    }
}

fn criterion(dir: &Path, out: &mut String) -> usize {
    let mut found = Baselines::new();
    baselines(dir, dir, &mut found);
    if found.is_empty() {
        let _ = writeln!(
            out,
            "MISSING  criterion: no readable base-N or head-N baseline in {}",
            dir.display()
        );
        return 1;
    }
    let mut failed = 0;
    for (name, runs) in found {
        if runs.keys().all(|(side, _)| *side == "head") {
            let _ = writeln!(out, "new      criterion {name}");
            continue;
        }
        let pairs: Vec<(f64, f64)> = runs
            .iter()
            .filter(|((side, _), _)| *side == "base")
            .filter_map(|(&(_, n), &old)| Some((old, *runs.get(&("head", n))?)))
            .collect();
        if pairs.len() < ROUNDS {
            failed += 1;
            let _ = writeln!(
                out,
                "MISSING  criterion {name}: {} readable rounds on main and now, {ROUNDS} needed",
                pairs.len()
            );
            continue;
        }
        let changes = pairs
            .iter()
            .filter_map(|&(old, new)| regression(Goal::Below(0.0), old, new))
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
            "{verdict:8} criterion {name}: {} ns -> {} ns ({:+.1} %, median of {} paired rounds)",
            number(old),
            number(new),
            change * 100.0,
            pairs.len()
        );
    }
    failed
}

pub(crate) fn run(target: &Path, rounds: Option<&Path>, cpu: Option<&str>) -> ExitCode {
    let (failed, out) = compare(target, rounds, cpu);
    let _ = std::io::stdout().lock().write_all(out.as_bytes());
    if failed == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

pub(crate) fn compare(target: &Path, dir: Option<&Path>, cpu: Option<&str>) -> (usize, String) {
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
            failed += absolute(&rounds, cpu, &mut out);
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
