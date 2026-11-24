#[expect(dead_code, reason = "the bench's entry point is not called from tests")]
#[path = "../benches/targets/gate.rs"]
mod gate;
#[expect(dead_code, reason = "the table printer is not called from tests")]
#[path = "../benches/targets/row.rs"]
mod row;

use std::path::Path;

use row::{CALIBRATION, Goal, REFERENCE_CALIBRATION_US, REFERENCE_CPU, Row};
use serde_json::{Value, json};
use tempfile::TempDir;

struct Measure {
    name: &'static str,
    goal: &'static str,
    target: f64,
    scaled: bool,
}

const LATENCY: Measure = Measure {
    name: "latency",
    goal: "below",
    target: 1000.0,
    scaled: false,
};
const THROUGHPUT: Measure = Measure {
    name: "throughput",
    goal: "above",
    target: 1000.0,
    scaled: false,
};

fn write(path: &Path, value: &Value) {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).unwrap_or_else(|e| panic!("{e}"));
    }
    std::fs::write(path, value.to_string()).unwrap_or_else(|e| panic!("{e}"));
}

fn report(rows: &[(&Measure, Option<f64>)], calibration: Option<f64>) -> Value {
    let mut rows: Vec<Value> = rows
        .iter()
        .map(|(m, value)| {
            json!({"measure": m.name, "goal": m.goal, "target": m.target, "unit": "µs",
                   "value": value, "scaled": m.scaled})
        })
        .collect();
    if let Some(c) = calibration {
        rows.push(json!({"measure": CALIBRATION, "goal": "report", "unit": "µs", "value": c}));
    }
    json!({"rows": rows})
}

fn round(dir: &Path, side: &str, n: u32, rows: &[(&Measure, Option<f64>)]) {
    let path = dir.join(format!("rounds/{side}-{n}.json"));
    write(&path, &report(rows, Some(REFERENCE_CALIBRATION_US)));
}

fn empty_rounds(dir: &Path) {
    for n in 1..=5 {
        round(dir, "base", n, &[]);
        round(dir, "head", n, &[]);
    }
}

fn criterion_run(dir: &Path, bench: &str, side: &str, n: u32, median: f64) {
    let baseline = dir.join(format!("criterion/{bench}/{side}-{n}"));
    write(
        &baseline.join("estimates.json"),
        &json!({"median": {"point_estimate": median}}),
    );
    write(&baseline.join("benchmark.json"), &json!({"title": bench}));
}

fn bench_list(dir: &Path, side: &str, benches: &[&str]) {
    let text: String = benches.iter().flat_map(|b| [*b, ": benchmark\n"]).collect();
    let path = dir.join(format!("criterion/expected-{side}.txt"));
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap_or_else(|e| panic!("{e}"));
    }
    std::fs::write(path, text).unwrap_or_else(|e| panic!("{e}"));
}

fn steady_criterion(dir: &Path) {
    if dir.join("criterion").exists() {
        return;
    }
    bench_list(dir, "base", &["steady"]);
    bench_list(dir, "head", &["steady"]);
    for n in 1..=5 {
        criterion_run(dir, "steady", "base", n, 100.0);
        criterion_run(dir, "steady", "head", n, 100.0);
    }
}

fn compare_on(dir: &Path, cpu: &str) -> usize {
    steady_criterion(dir);
    gate::compare(dir, Some(&dir.join("rounds")), Some(cpu)).0
}

fn compare(dir: &Path) -> usize {
    compare_on(dir, REFERENCE_CPU)
}

#[test]
fn a_row_meets_its_target_only_with_a_value_on_the_right_side() {
    // Given rows for a "below" and an "above" target, measured or not
    let below = |v| Row::new("latency", v, "µs", Goal::Below(100.0));
    let above = |v| Row::new("throughput", v, "req/s", Goal::Above(5000.0));

    // Then a value on the wrong side or no value at all is a miss, and a report row always meets
    assert!(below(Some(99.0)).met());
    assert!(!below(Some(100.0)).met());
    assert!(!below(None).met());
    assert!(above(Some(5001.0)).met());
    assert!(!above(Some(4999.0)).met());
    assert!(!above(None).met());
    assert!(Row::new("depth", None, "events", Goal::Report).met());
    assert_eq!(below(None).json()["met"], json!(false));
}

#[test]
fn one_slow_round_does_not_decide_a_row_but_a_median_change_over_20_percent_does() {
    // Given five paired rounds of a steady row with one slow head round, and of rows that regress
    let dir = TempDir::new().unwrap();
    let steady = Measure {
        name: "steady",
        ..LATENCY
    };
    let within = Measure {
        name: "slower within budget",
        ..LATENCY
    };
    let head = [
        (150.0, 125.0, 119.0, 3900.0),
        (100.0, 130.0, 119.0, 3900.0),
        (101.0, 90.0, 119.0, 6000.0),
        (100.0, 126.0, 119.0, 3900.0),
        (99.0, 80.0, 119.0, 6000.0),
    ];
    for (n, (s, slower, w, fewer)) in (1..).zip(head) {
        let base = [
            (&steady, Some(100.0)),
            (&LATENCY, Some(100.0)),
            (&within, Some(100.0)),
            (&THROUGHPUT, Some(5000.0)),
        ];
        round(dir.path(), "base", n, &base);
        let head = [
            (&steady, Some(s)),
            (&LATENCY, Some(slower)),
            (&within, Some(w)),
            (&THROUGHPUT, Some(fewer)),
        ];
        round(dir.path(), "head", n, &head);
    }

    // When the gate compares the rounds
    let failed = compare(dir.path());

    // Then the latency 25 % slower and the throughput 22 % lower in three of five rounds fail it
    assert_eq!(failed, 2);
}

#[test]
fn the_gate_needs_every_round_and_a_value_on_both_sides_of_every_row() {
    // Given a row main measured that the head drops, and a row the head did not measure in round 2
    let dir = TempDir::new().unwrap();
    let dropped = Measure {
        name: "dropped",
        ..LATENCY
    };
    for n in 1..=5 {
        round(
            dir.path(),
            "base",
            n,
            &[(&LATENCY, Some(10.0)), (&dropped, Some(10.0))],
        );
        let value = (n != 2).then_some(10.0);
        round(dir.path(), "head", n, &[(&LATENCY, value)]);
    }
    let unpaired = TempDir::new().unwrap();
    round(unpaired.path(), "head", 1, &[(&LATENCY, Some(10.0))]);

    // When the gate compares them, without a base for round 1, and without rounds at all
    let failed = compare(dir.path());
    let without_base = compare(unpaired.path());
    let (without_rounds, _) = gate::compare(dir.path(), None, Some(REFERENCE_CPU));

    // Then the dropped row, and the unmeasured round against main and against its target, fail it
    assert_eq!(failed, 3);
    assert_eq!(without_base, 1);
    assert_eq!(without_rounds, 1);
}

#[test]
fn a_scaled_target_is_judged_on_the_reference_runner_and_a_raw_one_as_measured() {
    // Given a runner half as fast as the reference, with scaled and raw rows that read twice the target
    let latency = Measure {
        name: "scaled latency",
        target: 100.0,
        scaled: true,
        ..LATENCY
    };
    let throughput = Measure {
        name: "scaled throughput",
        target: 5000.0,
        scaled: true,
        ..THROUGHPUT
    };
    let raw = Measure {
        name: "raw latency",
        target: 100.0,
        ..LATENCY
    };
    let rows = [
        (&latency, Some(180.0)),
        (&throughput, Some(3000.0)),
        (&raw, Some(180.0)),
    ];
    let judge = |calibration: Option<f64>| {
        let dir = TempDir::new().unwrap();
        for n in 1..=5 {
            for side in ["base", "head"] {
                write(
                    &dir.path().join(format!("rounds/{side}-{n}.json")),
                    &report(&rows, calibration),
                );
            }
        }
        compare(dir.path())
    };

    // When the gate judges them on a slower runner, a faster runner, and without the calibration
    let slower = judge(Some(2.0 * REFERENCE_CALIBRATION_US));
    let faster = judge(Some(0.5 * REFERENCE_CALIBRATION_US));
    let uncalibrated = judge(None);

    // Then the slower runner meets the scaled rows, the faster one misses them, and so does no calibration
    assert_eq!(slower, 1);
    assert_eq!(faster, 3);
    assert_eq!(uncalibrated, 3);
}

#[test]
fn a_criterion_bench_regresses_when_its_median_paired_change_passes_20_percent() {
    // Given five paired rounds of criterion medians, one bench with one slow head round, one 25 % slower
    let dir = TempDir::new().unwrap();
    let run = |bench: &str, side: &str, n: u32, median: f64| {
        criterion_run(dir.path(), bench, side, n, median);
    };
    let head = [
        (101.0, 125.0),
        (150.0, 130.0),
        (99.0, 100.0),
        (100.0, 126.0),
        (102.0, 90.0),
    ];
    for (n, (steady, slower)) in (1..).zip(head) {
        run("group/steady", "base", n, 100.0);
        run("group/steady", "head", n, steady);
        run("slower", "base", n, 100.0);
        run("slower", "head", n, slower);
        run("new", "head", n, 1.0);
    }
    bench_list(dir.path(), "base", &["group/steady", "slower"]);
    bench_list(dir.path(), "head", &["group/steady", "slower", "new"]);
    empty_rounds(dir.path());

    // When the gate compares them
    let failed = compare(dir.path());

    // Then only the bench that is 25 % slower in three of five rounds fails it
    assert_eq!(failed, 1);
}

type Bench<'a> = (&'a str, &'a [u32], &'a [u32]);

fn judge_criterion(
    benches: &[Bench<'_>],
    lists: Option<(&[&str], &[&str])>,
    unreadable: Option<&str>,
) -> usize {
    let dir = TempDir::new().unwrap_or_else(|e| panic!("{e}"));
    for (bench, base, head) in benches {
        for &n in *base {
            criterion_run(dir.path(), bench, "base", n, 100.0);
        }
        for &n in *head {
            criterion_run(dir.path(), bench, "head", n, 100.0);
        }
    }
    if let Some((base, head)) = lists {
        bench_list(dir.path(), "base", base);
        bench_list(dir.path(), "head", head);
    }
    if let Some(path) = unreadable {
        write(&dir.path().join(path), &json!({"median": {}}));
    }
    empty_rounds(dir.path());
    gate::compare(
        dir.path(),
        Some(&dir.path().join("rounds")),
        Some(REFERENCE_CPU),
    )
    .0
}

#[test]
fn the_criterion_gate_needs_five_readable_rounds_of_every_listed_bench_on_each_side() {
    // Given the bench lists that each revision recorded before the runs, and their results
    let all: &[u32] = &[1, 2, 3, 4, 5];
    let steady: Bench<'_> = ("steady", all, all);
    let both: (&[&str], &[&str]) = (&["steady"], &["steady"]);
    let judge = judge_criterion;

    // When the gate judges each case
    let unlisted = judge(&[steady], None, None);
    let listed_nothing = judge(&[], Some((&[], &[])), None);
    let nothing_measured = judge(&[], Some(both), None);
    let complete = judge(&[steady], Some(both), None);
    let four_rounds = judge(&[("steady", all, &[1, 2, 3, 4])], Some(both), None);
    let unreadable = judge(
        &[steady],
        Some(both),
        Some("criterion/steady/head-3/estimates.json"),
    );
    let main_unmeasured = judge(&[("steady", &[], all)], Some(both), None);
    let dropped = judge(
        &[steady, ("dropped", all, &[])],
        Some((&["steady", "dropped"], &["steady"])),
        None,
    );
    let new_lists = (&["steady"][..], &["steady", "added"][..]);
    let added = judge(&[steady, ("added", &[], all)], Some(new_lists), None);
    let added_once = judge(&[steady, ("added", &[], &[1])], Some(new_lists), None);
    let stray = judge(&[steady, ("stray", all, all)], Some(both), None);

    // Then only five readable rounds of every listed bench pass, on main and now for a bench both
    // revisions list, and on the head alone for a bench new on the head
    assert_eq!(unlisted, 1);
    assert_eq!(listed_nothing, 1);
    assert_eq!(nothing_measured, 1);
    assert_eq!(complete, 0);
    assert_eq!(four_rounds, 1);
    assert_eq!(unreadable, 1);
    assert_eq!(main_unmeasured, 1);
    assert_eq!(dropped, 1);
    assert_eq!(added, 0);
    assert_eq!(added_once, 1);
    assert_eq!(stray, 1);
}

#[test]
fn the_targets_gate_needs_five_paired_rounds() {
    // Given four and five paired rounds of a row that meets its target on both sides
    let judge = |rounds: u32| {
        let dir = TempDir::new().unwrap();
        for n in 1..=rounds {
            round(dir.path(), "base", n, &[(&LATENCY, Some(10.0))]);
            round(dir.path(), "head", n, &[(&LATENCY, Some(10.0))]);
        }
        compare(dir.path())
    };

    // When the gate judges them
    let four = judge(4);
    let five = judge(5);

    // Then four rounds fail it, as the criterion gate fails four rounds of a bench
    assert_eq!(four, 1);
    assert_eq!(five, 0);
}

#[test]
fn a_scaled_miss_off_the_reference_model_fails_only_when_main_meets_the_target() {
    // Given a scaled row and a raw row that the head misses, against a main that meets or misses them
    let scaled = Measure {
        name: "scaled latency",
        target: 100.0,
        scaled: true,
        ..LATENCY
    };
    let raw = Measure {
        name: "raw latency",
        target: 100.0,
        ..LATENCY
    };
    let judge = |measure: &Measure, base: Option<f64>, cpu: &str| {
        let dir = TempDir::new().unwrap();
        for n in 1..=5 {
            let rows = base.map(|b| (measure, Some(b)));
            round(dir.path(), "base", n, rows.as_slice());
            round(dir.path(), "head", n, &[(measure, Some(104.0))]);
        }
        compare_on(dir.path(), cpu)
    };
    let other = "AMD EPYC 7763 64-Core Processor";

    // When the gate judges them on the reference model and on another model
    let reference_both_miss = judge(&scaled, Some(103.0), "intel(r) xeon(r) 6975p-c ");
    let other_base_meets = judge(&scaled, Some(90.0), other);
    let other_both_miss = judge(&scaled, Some(103.0), other);
    let other_base_absent = judge(&scaled, None, other);
    let other_raw_both_miss = judge(&raw, Some(103.0), other);

    // Then only a scaled row that main misses too on another model passes, as a runner miss
    assert_eq!(reference_both_miss, 1);
    assert_eq!(other_base_meets, 1);
    assert_eq!(other_both_miss, 0);
    assert_eq!(other_base_absent, 1);
    assert_eq!(other_raw_both_miss, 1);
}
