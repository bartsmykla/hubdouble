use std::collections::HashMap;

use crate::store::{Table, TableMut};

/// A generated sequence. Every one keeps counting up across a reset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Sequence {
    /// The global `int64` ID of every object.
    Id,
    Delivery,
    Run,
    LogCursor,
    /// The ID of each activity event, apart from [`Sequence::Id`] so that recording an event
    /// does not move the IDs of other objects.
    Event,
}

/// The counter table: the generated sequences, each repository's issue and PR number, and the
/// seed ceiling. Its [`Table::restore`] keeps the current values.
#[derive(Debug, Clone, Default)]
pub struct Counters {
    values: HashMap<Sequence, i64>,
    numbers: HashMap<i64, i64>,
    seeded_below: i64,
}

impl Table for Counters {
    fn restore(&mut self, _seeded: &Counters) {}
}

impl Counters {
    /// The last value handed out, `0` before the first.
    #[must_use]
    pub fn current(&self, s: Sequence) -> i64 {
        self.values.get(&s).copied().unwrap_or(0)
    }

    /// The last issue or PR number handed out in a repository, `0` before the first.
    #[must_use]
    pub fn number(&self, repo_id: i64) -> i64 {
        self.numbers.get(&repo_id).copied().unwrap_or(0)
    }

    /// Whether an object ID was handed out before [`TableMut::mark_seeded`], that is, by the seed load.
    #[must_use]
    pub fn is_seeded(&self, id: i64) -> bool {
        id < self.seeded_below
    }
}

impl TableMut<'_, Counters> {
    pub fn next(&mut self, s: Sequence) -> i64 {
        let old = self.table.current(s);
        self.table.values.insert(s, old + 1);
        self.record(move |c| {
            c.values.insert(s, old);
        });
        old + 1
    }

    /// Moves `s` up so the next value is above `at_least`; a lower value changes nothing.
    pub fn bump(&mut self, s: Sequence, at_least: i64) {
        let old = self.table.current(s);
        if at_least > old {
            self.table.values.insert(s, at_least);
            self.record(move |c| {
                c.values.insert(s, old);
            });
        }
    }

    pub fn next_number(&mut self, repo_id: i64) -> i64 {
        let old = self.table.number(repo_id);
        self.table.numbers.insert(repo_id, old + 1);
        self.record(move |c| {
            c.numbers.insert(repo_id, old);
        });
        old + 1
    }

    pub fn bump_number(&mut self, repo_id: i64, at_least: i64) {
        let old = self.table.number(repo_id);
        if at_least > old {
            self.table.numbers.insert(repo_id, at_least);
            self.record(move |c| {
                c.numbers.insert(repo_id, old);
            });
        }
    }

    /// Records that every object ID handed out so far belongs to the seed.
    pub fn mark_seeded(&mut self) {
        let old = self.table.seeded_below;
        self.table.seeded_below = self.table.current(Sequence::Id) + 1;
        self.record(move |c| c.seeded_below = old);
    }
}
