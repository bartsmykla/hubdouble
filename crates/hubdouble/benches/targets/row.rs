use serde_json::{Value, json};

const UNITS: [&str; 9] = [
    "",
    "µs",
    "ms",
    "MB",
    "MiB",
    "conns",
    "deliveries",
    "events",
    "req/s",
];

#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Goal {
    Below(f64),
    Above(f64),
    Report,
}

pub(crate) const CALIBRATION: &str = "Runner calibration: JSON round trip of 13 KB";
pub(crate) const INVENTORY_TARGET: &str = "Inventory GraphQL document";
pub(crate) const INVENTORY_DIR: &str =
    concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/inventory/graphql");

/// The sorted names of the inventory GraphQL documents, each of which needs its own row.
pub(crate) fn inventory_documents() -> std::io::Result<Vec<String>> {
    let mut names: Vec<String> = std::fs::read_dir(INVENTORY_DIR)?
        .filter_map(Result::ok)
        .filter_map(|e| {
            e.file_name()
                .to_str()
                .and_then(|n| n.strip_suffix(".graphql"))
                .map(str::to_owned)
        })
        .collect();
    names.sort();
    Ok(names)
}

/// One target of plan section 9: the rows that measure it, and the goal and unit each of
/// those rows must report, so the gate judges the plan's target and not the report's.
pub(crate) struct Section9 {
    pub(crate) target: &'static str,
    pub(crate) prefix: &'static str,
    pub(crate) suffix: &'static str,
    pub(crate) goal: Goal,
    pub(crate) unit: &'static str,
}

impl Section9 {
    pub(crate) fn names(&self, measure: &str) -> bool {
        measure.len() >= self.prefix.len() + self.suffix.len()
            && measure.starts_with(self.prefix)
            && measure.ends_with(self.suffix)
    }
}

const fn exact(
    target: &'static str,
    measure: &'static str,
    goal: Goal,
    unit: &'static str,
) -> Section9 {
    Section9 {
        target,
        prefix: measure,
        suffix: "",
        goal,
        unit,
    }
}

pub(crate) const SECTION_9: [Section9; 11] = [
    exact("Image size", "Image size", Goal::Below(20.0), "MB"),
    exact(
        "Start to ready, fixture seed",
        "Start to ready, fixture seed",
        Goal::Below(200.0),
        "ms",
    ),
    exact(
        "Idle memory",
        "Idle RSS, client seed",
        Goal::Below(40.0),
        "MB",
    ),
    Section9 {
        target: "REST GET p50",
        prefix: "REST GET p50, ",
        suffix: " routes",
        goal: Goal::Below(100.0),
        unit: "µs",
    },
    Section9 {
        target: "REST GET p99",
        prefix: "REST GET p99, ",
        suffix: " routes",
        goal: Goal::Below(1000.0),
        unit: "µs",
    },
    Section9 {
        target: INVENTORY_TARGET,
        prefix: "GraphQL ",
        suffix: " p99",
        goal: Goal::Below(3.0),
        unit: "ms",
    },
    Section9 {
        target: "Push to a base branch with 100 open pull requests",
        prefix: "GraphQL updateRefs p99, main with ",
        suffix: " open pull requests",
        goal: Goal::Below(3.0),
        unit: "ms",
    },
    Section9 {
        target: "REST read of a pull request, the first after each such push",
        prefix: "REST pull request read p99, first after a push to main with ",
        suffix: " open pull requests",
        goal: Goal::Below(1000.0),
        unit: "µs",
    },
    exact(
        "Mixed inventory load, reads and writes",
        "Mixed inventory load, reads and writes",
        Goal::Above(5000.0),
        "req/s",
    ),
    exact(
        "Webhook enqueue",
        "Webhook enqueue per event",
        Goal::Below(20.0),
        "µs",
    ),
    exact("Runner calibration", CALIBRATION, Goal::Report, "µs"),
];
pub(crate) const REFERENCE_CALIBRATION_US: f64 = 54.8;
pub(crate) const REFERENCE_CPU: &str = "Intel(R) Xeon(R) 6975P-C";

pub(crate) struct Row {
    pub(crate) measure: String,
    pub(crate) value: Option<f64>,
    pub(crate) unit: &'static str,
    pub(crate) goal: Goal,
    pub(crate) scaled: bool,
    pub(crate) note: String,
}

impl Row {
    pub(crate) fn new(
        measure: impl Into<String>,
        value: Option<f64>,
        unit: &'static str,
        goal: Goal,
    ) -> Row {
        Row {
            measure: measure.into(),
            value,
            unit,
            goal,
            scaled: false,
            note: String::new(),
        }
    }

    pub(crate) fn note(mut self, note: impl Into<String>) -> Row {
        self.note = note.into();
        self
    }

    pub(crate) fn scaled(mut self) -> Row {
        self.scaled = true;
        self
    }

    pub(crate) fn met(&self) -> bool {
        self.met_by(self.value)
    }

    pub(crate) fn met_by(&self, value: Option<f64>) -> bool {
        match (self.goal, value) {
            (Goal::Report, _) => true,
            (_, None) => false,
            (Goal::Below(t), Some(v)) => v < t,
            (Goal::Above(t), Some(v)) => v > t,
        }
    }

    pub(crate) fn target(&self) -> String {
        match self.goal {
            Goal::Below(t) => format!("< {} {}", number(t), self.unit),
            Goal::Above(t) => format!("> {} {}", number(t), self.unit),
            Goal::Report => "report".to_owned(),
        }
    }

    pub(crate) fn json(&self) -> Value {
        let (goal, target) = match self.goal {
            Goal::Below(t) => ("below", Some(t)),
            Goal::Above(t) => ("above", Some(t)),
            Goal::Report => ("report", None),
        };
        json!({"measure": self.measure, "value": self.value, "unit": self.unit,
               "goal": goal, "target": target, "met": self.met(), "note": self.note,
               "scaled": self.scaled})
    }

    pub(crate) fn from_json(v: &Value) -> Option<Row> {
        let unit = UNITS.into_iter().find(|u| v["unit"] == *u)?;
        let goal = match (v["goal"].as_str()?, v["target"].as_f64()) {
            ("below", Some(t)) => Goal::Below(t),
            ("above", Some(t)) => Goal::Above(t),
            ("report", _) => Goal::Report,
            _ => return None,
        };
        let mut row = Row::new(v["measure"].as_str()?, v["value"].as_f64(), unit, goal);
        row.scaled = v["scaled"].as_bool().unwrap_or(false);
        Some(row.note(v["note"].as_str().unwrap_or_default()))
    }
}

pub(crate) fn number(v: f64) -> String {
    if v >= 100.0 {
        format!("{v:.0}")
    } else if v >= 10.0 {
        format!("{v:.1}")
    } else {
        format!("{v:.2}")
    }
}
