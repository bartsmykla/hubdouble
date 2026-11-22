use std::collections::VecDeque;

use parking_lot::Mutex;
use serde::Serialize;
use tokio::sync::Notify;

use crate::LogEntry;

pub(crate) const CAPACITY: usize = 50_000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct Logged {
    pub cursor: u64,
    pub kind: &'static str,
    pub method: String,
    pub path: String,
    pub operation_id: String,
    pub actor: Option<String>,
    pub status: u16,
}

impl Logged {
    fn matches(&self, patterns: &[String]) -> bool {
        if patterns.is_empty() {
            return true;
        }
        let line = format!(
            "{} {} {} {} {}",
            self.method,
            self.path,
            self.operation_id,
            self.status,
            self.actor.as_deref().unwrap_or("")
        );
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
        let mut ring = self.ring.lock();
        ring.last += 1;
        let logged = Logged {
            cursor: ring.last,
            kind: "request",
            method: e.method.to_owned(),
            path: e.path.to_owned(),
            operation_id: e.operation_id.to_owned(),
            actor: e.actor.map(str::to_owned),
            status: e.status,
        };
        ring.entries.push_back(logged);
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
        assert_eq!(
            all.iter().map(|e| e.path.as_str()).collect::<Vec<_>>(),
            ["/b", "/c"]
        );
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
        assert_eq!((got.path.as_str(), got.cursor), ("/repos/o/r", cursor + 2));
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
