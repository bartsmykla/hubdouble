#[expect(dead_code, reason = "the bench's entry point is not called from tests")]
#[path = "../benches/targets/gate.rs"]
mod gate;
#[expect(dead_code, reason = "the table printer is not called from tests")]
#[path = "../benches/targets/row.rs"]
mod row;

use std::path::Path;

use row::{Goal, Row};
use serde_json::{Value, json};
use tempfile::TempDir;

fn write(path: &Path, value: &Value) {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).unwrap_or_else(|e| panic!("{e}"));
    }
    std::fs::write(path, value.to_string()).unwrap_or_else(|e| panic!("{e}"));
}

fn rows(values: &[(&str, &str, Option<f64>)]) -> Value {
    json!({"rows": values
        .iter()
        .map(|(measure, goal, value)| json!({"measure": measure, "goal": goal, "value": value}))
        .collect::<Vec<_>>()})
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
fn the_gate_fails_a_target_that_regresses_more_than_20_percent_against_main() {
    // Given main's targets and a run that moves each by a known amount
    let dir = TempDir::new().unwrap();
    let base = dir.path().join("main.json");
    let current = dir.path().join("current.json");
    write(
        &base,
        &rows(&[
            ("slower within budget", "below", Some(100.0)),
            ("slower beyond budget", "below", Some(100.0)),
            ("fewer within budget", "above", Some(5000.0)),
            ("fewer beyond budget", "above", Some(5000.0)),
            ("no longer measured", "below", Some(1.0)),
            ("reported", "report", Some(10.0)),
        ]),
    );
    write(
        &current,
        &rows(&[
            ("slower within budget", "below", Some(119.0)),
            ("slower beyond budget", "below", Some(121.0)),
            ("fewer within budget", "above", Some(4100.0)),
            ("fewer beyond budget", "above", Some(3900.0)),
            ("no longer measured", "below", None),
            ("reported", "report", Some(1000.0)),
            ("new measure", "below", Some(5.0)),
        ]),
    );

    // When the gate compares them
    let (failed, _) = gate::compare(dir.path(), &current, Some(&base));

    // Then the two regressions beyond budget and the lost measure fail it
    assert_eq!(failed, 3);
}

#[test]
fn the_gate_fails_a_criterion_bench_whose_slowdown_lower_bound_passes_the_budget() {
    // Given criterion change estimates against main's baseline
    let dir = TempDir::new().unwrap();
    let change = |name: &str, lower: f64| {
        write(
            &dir.path()
                .join("criterion")
                .join(name)
                .join("change/estimates.json"),
            &json!({"mean": {"point_estimate": lower + 0.05,
                             "confidence_interval": {"lower_bound": lower}}}),
        );
    };
    change("steady", 0.19);
    change("faster", -0.30);
    change("slower", 0.21);

    // When the gate runs without a targets baseline
    let (failed, _) = gate::compare(dir.path(), &dir.path().join("none.json"), None);

    // Then only the bench that is surely more than 20 % slower fails it
    assert_eq!(failed, 1);
}
