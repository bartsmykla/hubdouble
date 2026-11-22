use std::collections::VecDeque;

use hubdouble_hooks::Notice;
use parking_lot::Mutex;
use serde::Serialize;
use tokio::sync::Notify;

use crate::LogEntry;

pub(crate) const CAPACITY: usize = 50_000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct Logged {
    pub cursor: u64,
    #[serde(flatten)]
    pub what: What,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub(crate) enum What {
    Request {
        method: String,
        path: String,
        operation_id: String,
        actor: Option<String>,
        status: u16,
    },
    Event {
        app_id: i64,
        delivery_id: i64,
        event: String,
        action: Option<String>,
    },
    Delivery {
        app_id: i64,
        delivery_id: i64,
        event: String,
        action: Option<String>,
        status_code: i64,
    },
}

fn event_name(event: &str, action: Option<&str>) -> String {
    match action {
        Some(a) => format!("{event}.{a}"),
        None => event.to_owned(),
    }
}

impl What {
    fn line(&self) -> String {
        match self {
            What::Request {
                method,
                path,
                operation_id,
                actor,
                status,
            } => format!(
                "{method} {path} {operation_id} {status} {}",
                actor.as_deref().unwrap_or("")
            ),
            What::Event {
                app_id,
                delivery_id,
                event,
                action,
            } => format!(
                "event {} {app_id} {delivery_id}",
                event_name(event, action.as_deref())
            ),
            What::Delivery {
                app_id,
                delivery_id,
                event,
                action,
                status_code,
            } => format!(
                "delivery {} {status_code} {app_id} {delivery_id}",
                event_name(event, action.as_deref())
            ),
        }
    }

    fn of(notice: &Notice<'_>) -> What {
        match *notice {
            Notice::Event {
                app_id,
                delivery_id,
                event,
                action,
            } => What::Event {
                app_id,
                delivery_id,
                event: event.to_owned(),
                action: action.map(str::to_owned),
            },
            Notice::Delivery {
                app_id,
                delivery_id,
                event,
                action,
                status_code,
            } => What::Delivery {
                app_id,
                delivery_id,
                event: event.to_owned(),
                action: action.map(str::to_owned),
                status_code,
            },
        }
    }
}

impl Logged {
    fn matches(&self, patterns: &[String]) -> bool {
        if patterns.is_empty() {
            return true;
        }
        let line = self.what.line();
        patterns.iter().all(|p| line.contains(p.as_str()))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Gone;

struct Ring {
    entries: VecDeque<Logged>,
    last: u64,
    floor: u64,
}

pub(crate) struct Log {
    ring: Mutex<Ring>,
    added: Notify,
    capacity: usize,
}

impl Default for Log {
    fn default() -> Log {
        Log::with_capacity(CAPACITY)
    }
}

impl Log {
    pub(crate) fn with_capacity(capacity: usize) -> Log {
        Log {
            ring: Mutex::new(Ring {
                entries: VecDeque::new(),
                last: 0,
                floor: 0,
            }),
            added: Notify::new(),
            capacity,
        }
    }

    pub(crate) fn record(&self, e: &LogEntry<'_>) {
        self.push(What::Request {
            method: e.method.to_owned(),
            path: e.path.to_owned(),
            operation_id: e.operation_id.to_owned(),
            actor: e.actor.map(str::to_owned),
            status: e.status,
        });
    }

    pub(crate) fn note(&self, notice: &Notice<'_>) {
        self.push(What::of(notice));
    }

    fn push(&self, what: What) {
        let mut ring = self.ring.lock();
        ring.last += 1;
        let cursor = ring.last;
        ring.entries.push_back(Logged { cursor, what });
        if ring.entries.len() > self.capacity
            && let Some(dropped) = ring.entries.pop_front()
        {
            ring.floor = dropped.cursor;
        }
        drop(ring);
        self.added.notify_waiters();
    }

    pub(crate) fn reset(&self) {
        let mut ring = self.ring.lock();
        ring.entries.clear();
        ring.last += 1;
        ring.floor = ring.last;
        drop(ring);
        self.added.notify_waiters();
    }

    pub(crate) fn since(&self, after: Option<u64>) -> Result<(Vec<Logged>, u64), Gone> {
        let ring = self.ring.lock();
        let after = check(&ring, after)?;
        let entries = ring
            .entries
            .iter()
            .filter(|e| e.cursor > after)
            .cloned()
            .collect();
        Ok((entries, ring.last))
    }

    pub(crate) async fn wait(
        &self,
        after: Option<u64>,
        patterns: &[String],
    ) -> Result<Logged, Gone> {
        let after = check(&self.ring.lock(), after)?;
        loop {
            let added = self.added.notified();
            tokio::pin!(added);
            added.as_mut().enable();
            {
                let ring = self.ring.lock();
                if after < ring.floor {
                    return Err(Gone);
                }
                if let Some(e) = ring
                    .entries
                    .iter()
                    .find(|e| e.cursor > after && e.matches(patterns))
                {
                    return Ok(e.clone());
                }
            }
            added.await;
        }
    }
}

fn check(ring: &Ring, after: Option<u64>) -> Result<u64, Gone> {
    match after {
        None => Ok(ring.floor),
        Some(a) if a < ring.floor => Err(Gone),
        Some(a) => Ok(a),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    fn path_of(l: &Logged) -> &str {
        match &l.what {
            What::Request { path, .. } => path,
            What::Event { .. } | What::Delivery { .. } => "",
        }
    }

    fn entry(path: &str) -> LogEntry<'_> {
        LogEntry {
            method: "GET",
            path,
            operation_id: "repos/get",
            actor: Some("alice"),
            status: 200,
        }
    }

    #[test]
    fn a_cursor_older_than_the_window_is_gone() {
        // Given
        let log = Log::with_capacity(2);
        for p in ["/a", "/b", "/c"] {
            log.record(&entry(p));
        }
        // When
        let (all, cursor) = log.since(None).unwrap();
        // Then
        assert_eq!(all.iter().map(path_of).collect::<Vec<_>>(), ["/b", "/c"]);
        assert_eq!(cursor, 3);
        assert_eq!(log.since(Some(0)), Err(Gone));
        assert_eq!(log.since(Some(1)).unwrap().0.len(), 2);
        assert_eq!(log.since(Some(3)).unwrap().0.len(), 0);
    }

    #[test]
    fn reset_empties_the_log_and_refuses_older_cursors_but_keeps_counting() {
        // Given
        let log = Log::default();
        log.record(&entry("/a"));
        let (_, before) = log.since(None).unwrap();
        // When
        log.reset();
        // Then
        assert_eq!(log.since(Some(before)), Err(Gone));
        let (empty, cursor) = log.since(None).unwrap();
        assert_eq!(empty, []);
        assert!(cursor > before);
        log.record(&entry("/b"));
        let (after, _) = log.since(Some(cursor)).unwrap();
        assert_eq!(after.len(), 1);
        assert!(after[0].cursor > cursor);
    }

    #[tokio::test(start_paused = true)]
    async fn wait_returns_the_first_matching_entry_after_the_cursor() {
        // Given
        let log = Arc::new(Log::default());
        log.record(&entry("/repos/o/r"));
        let (_, cursor) = log.since(None).unwrap();
        let waiting = {
            let log = log.clone();
            tokio::spawn(async move {
                log.wait(Some(cursor), &["/repos/o/r".to_owned(), "alice".to_owned()])
                    .await
            })
        };
        tokio::task::yield_now().await;
        // When
        log.record(&entry("/other"));
        log.record(&entry("/repos/o/r"));
        // Then
        let got = waiting.await.unwrap().unwrap();
        assert_eq!((path_of(&got), got.cursor), ("/repos/o/r", cursor + 2));
    }

    #[tokio::test(start_paused = true)]
    async fn a_wait_held_across_a_reset_is_gone() {
        // Given
        let log = Arc::new(Log::default());
        let waiting = {
            let log = log.clone();
            tokio::spawn(async move { log.wait(Some(0), &["never".to_owned()]).await })
        };
        tokio::task::yield_now().await;
        // When
        log.reset();
        // Then
        assert_eq!(waiting.await.unwrap(), Err(Gone));
    }
}
