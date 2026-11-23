#[expect(dead_code, reason = "the bench's entry point is not called from tests")]
#[path = "../benches/targets/gate.rs"]
mod gate;
#[expect(dead_code, reason = "the table printer is not called from tests")]
#[path = "../benches/targets/row.rs"]
mod row;

use std::path::Path;

use row::{CALIBRATION, Goal, REFERENCE_CALIBRATION_US, Row};
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

fn compare(dir: &Path) -> usize {
    gate::compare(dir, Some(&dir.join("rounds"))).0
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
    // Given three paired rounds of a steady row with one slow head round, and of rows that regress
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

    // Then the latency 25 % slower and the throughput 22 % lower in two of three rounds fail it
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
    for n in 1..=2 {
        round(
            dir.path(),
            "base",
            n,
            &[(&LATENCY, Some(10.0)), (&dropped, Some(10.0))],
        );
        let value = (n == 1).then_some(10.0);
        round(dir.path(), "head", n, &[(&LATENCY, value)]);
    }
    let unpaired = TempDir::new().unwrap();
    round(unpaired.path(), "head", 1, &[(&LATENCY, Some(10.0))]);

    // When the gate compares them, without a base for round 1, and without rounds at all
    let failed = compare(dir.path());
    let without_base = compare(unpaired.path());
    let (without_rounds, _) = gate::compare(dir.path(), None);

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
        write(
            &dir.path().join("rounds/base-1.json"),
            &report(&rows, calibration),
        );
        write(
            &dir.path().join("rounds/head-1.json"),
            &report(&rows, calibration),
        );
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
    // Given three paired rounds of criterion medians, one bench with one slow head round, one 25 % slower
    let dir = TempDir::new().unwrap();
    let run = |bench: &str, side: &str, n: u32, median: f64| {
        let file = format!("criterion/{bench}/{side}-{n}/estimates.json");
        write(
            &dir.path().join(file),
            &json!({"median": {"point_estimate": median}}),
        );
    };
    for (n, steady, slower) in [(1, 101.0, 125.0), (2, 150.0, 130.0), (3, 99.0, 100.0)] {
        run("group/steady", "base", n, 100.0);
        run("group/steady", "head", n, steady);
        run("slower", "base", n, 100.0);
        run("slower", "head", n, slower);
    }
    run("new", "head", 1, 1.0);
    round(dir.path(), "base", 1, &[]);
    round(dir.path(), "head", 1, &[]);

    // When the gate compares them
    let failed = compare(dir.path());

    // Then only the bench that is 25 % slower in two of three rounds fails it
    assert_eq!(failed, 1);
}
