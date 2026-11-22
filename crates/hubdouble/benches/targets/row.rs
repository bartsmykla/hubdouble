use serde_json::{Value, json};

#[derive(Clone, Copy)]
pub(crate) enum Goal {
    Below(f64),
    Above(f64),
    Report,
}

pub(crate) struct Row {
    pub(crate) measure: String,
    pub(crate) value: Option<f64>,
    pub(crate) unit: &'static str,
    pub(crate) goal: Goal,
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
            note: String::new(),
        }
    }

    pub(crate) fn note(mut self, note: impl Into<String>) -> Row {
        self.note = note.into();
        self
    }

    pub(crate) fn met(&self) -> bool {
        match (self.goal, self.value) {
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
               "goal": goal, "target": target, "met": self.met()})
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
