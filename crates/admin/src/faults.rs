use std::collections::BTreeMap;
use std::time::Duration;

use bytes::Bytes;
use hubdouble_model::ApiError;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::{Fault, FaultQuery};

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FaultSpec {
    #[serde(default)]
    pub method: Option<String>,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub operation: Option<String>,
    pub status: u16,
    #[serde(default)]
    pub body: Option<serde_json::Value>,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default)]
    pub delay_ms: u64,
    #[serde(default)]
    pub count: Option<u32>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct Rule {
    pub id: u64,
    #[serde(flatten)]
    pub spec: FaultSpec,
    #[serde(skip)]
    body: Bytes,
}

#[derive(Default)]
pub(crate) struct Faults {
    rules: Mutex<(u64, Vec<Rule>)>,
}

impl Faults {
    pub(crate) fn add(&self, spec: FaultSpec) -> Result<u64, ApiError> {
        if !(100..=599).contains(&spec.status) {
            return Err(ApiError::unprocessable(
                "status must be between 100 and 599",
            ));
        }
        let body = match &spec.body {
            None => Bytes::new(),
            Some(serde_json::Value::String(s)) => Bytes::from(s.clone()),
            Some(v) => Bytes::from(v.to_string()),
        };
        let mut rules = self.rules.lock();
        rules.0 += 1;
        let id = rules.0;
        rules.1.push(Rule { id, spec, body });
        Ok(id)
    }

    pub(crate) fn list(&self) -> Vec<Rule> {
        self.rules.lock().1.clone()
    }

    pub(crate) fn clear(&self) {
        self.rules.lock().1.clear();
    }

    pub(crate) fn take(&self, q: &FaultQuery<'_>) -> Option<Fault> {
        let mut rules = self.rules.lock();
        let i = rules.1.iter().position(|r| r.matches(q))?;
        let rule = &mut rules.1[i];
        let fault = Fault {
            status: rule.spec.status,
            body: rule.body.clone(),
            headers: rule.spec.headers.clone().into_iter().collect(),
            delay: Duration::from_millis(rule.spec.delay_ms),
        };
        if let Some(n) = &mut rule.spec.count {
            *n -= 1;
            if *n == 0 {
                rules.1.remove(i);
            }
        }
        Some(fault)
    }
}

impl Rule {
    fn matches(&self, q: &FaultQuery<'_>) -> bool {
        let s = &self.spec;
        s.count != Some(0)
            && s.method
                .as_deref()
                .is_none_or(|m| m.eq_ignore_ascii_case(q.method))
            && s.operation.as_deref().is_none_or(|o| o == q.operation)
            && s.path.as_deref().is_none_or(|p| {
                glob(p, q.path)
                    || q.path
                        .strip_prefix("/api/v3")
                        .is_some_and(|rest| glob(p, rest))
            })
    }
}

fn glob(pattern: &str, text: &str) -> bool {
    let mut parts = pattern.split('*');
    let Some(first) = parts.next() else {
        return text.is_empty();
    };
    let Some(mut rest) = text.strip_prefix(first) else {
        return false;
    };
    let parts: Vec<&str> = parts.collect();
    let Some((last, middle)) = parts.split_last() else {
        return rest.is_empty();
    };
    for part in middle {
        match rest.find(part) {
            Some(at) => rest = &rest[at + part.len()..],
            None => return false,
        }
    }
    rest.len() >= last.len() && rest.ends_with(last)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(json: serde_json::Value) -> FaultSpec {
        serde_json::from_value(json).unwrap()
    }

    fn query<'a>(method: &'a str, path: &'a str, operation: &'a str) -> FaultQuery<'a> {
        FaultQuery {
            method,
            path,
            operation,
        }
    }

    #[test]
    fn globs_match_any_text_at_each_star() {
        assert!(glob("/repos/*/pulls", "/repos/o/r/pulls"));
        assert!(glob("/repos/*", "/repos/o/r"));
        assert!(glob("*", ""));
        assert!(glob("/a*b*c", "/aXbYc"));
        assert!(!glob("/repos/*/pulls", "/repos/o/r/pulls/1"));
        assert!(!glob("/a*aa", "/aa"));
        assert!(!glob("/exact", "/exact/more"));
    }

    #[test]
    fn a_counted_fault_answers_that_many_matching_requests() {
        // Given
        let faults = Faults::default();
        faults
            .add(spec(serde_json::json!({
                "method": "get", "path": "/repos/*", "status": 502, "count": 2,
                "body": {"message": "Server Error"}, "headers": {"retry-after": "1"}, "delay_ms": 5
            })))
            .unwrap();
        // When
        let miss = faults.take(&query("POST", "/repos/o/r", "repos/update"));
        let first = faults.take(&query("GET", "/api/v3/repos/o/r", "repos/get"));
        let second = faults.take(&query("GET", "/repos/o/r", "repos/get"));
        let third = faults.take(&query("GET", "/repos/o/r", "repos/get"));
        // Then
        assert!(miss.is_none());
        let first = first.unwrap();
        assert_eq!(first.status, 502);
        assert_eq!(first.delay, Duration::from_millis(5));
        assert_eq!(first.headers, [("retry-after".to_owned(), "1".to_owned())]);
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&first.body).unwrap(),
            serde_json::json!({"message": "Server Error"})
        );
        assert!(second.is_some());
        assert!(third.is_none());
        assert!(faults.list().is_empty());
    }

    #[test]
    fn a_fault_matches_a_graphql_operation_name() {
        let faults = Faults::default();
        faults
            .add(spec(
                serde_json::json!({"operation": "proposals", "status": 403}),
            ))
            .unwrap();
        assert!(faults.take(&query("POST", "/graphql", "other")).is_none());
        assert_eq!(
            faults
                .take(&query("POST", "/graphql", "proposals"))
                .unwrap()
                .status,
            403
        );
        assert!(
            faults
                .take(&query("POST", "/graphql", "proposals"))
                .is_some()
        );
    }

    #[test]
    fn a_status_outside_http_is_refused() {
        let e = Faults::default()
            .add(spec(serde_json::json!({"status": 42})))
            .unwrap_err();
        assert_eq!(e.status, 422);
    }
}
