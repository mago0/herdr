//! Finds agents that report working while their screen stays unchanged.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::terminal::TerminalId;

#[derive(Debug, Clone, Copy)]
struct StallWatch {
    seq: u64,
    changed_at: Instant,
    stalled: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StallChange {
    Entered { quiet_secs: u64 },
    Left { quiet_secs: u64 },
}

#[derive(Debug, Default)]
pub(crate) struct StallTable {
    watches: HashMap<TerminalId, StallWatch>,
}

impl StallTable {
    /// Records one sample of a terminal. `seq` is its screen content counter.
    pub(crate) fn observe(
        &mut self,
        terminal: &TerminalId,
        working: bool,
        seq: u64,
        now: Instant,
        threshold: Option<Duration>,
    ) -> Option<StallChange> {
        let Some(threshold) = threshold.filter(|_| working) else {
            return self.forget(terminal, now);
        };
        let watch = self.watches.entry(terminal.clone()).or_insert(StallWatch {
            seq,
            changed_at: now,
            stalled: false,
        });
        let quiet = now.saturating_duration_since(watch.changed_at);
        if watch.seq != seq {
            let was_stalled = watch.stalled;
            *watch = StallWatch {
                seq,
                changed_at: now,
                stalled: false,
            };
            return was_stalled.then_some(StallChange::Left {
                quiet_secs: quiet.as_secs(),
            });
        }
        if !watch.stalled && quiet >= threshold {
            watch.stalled = true;
            return Some(StallChange::Entered {
                quiet_secs: quiet.as_secs(),
            });
        }
        None
    }

    pub(crate) fn forget(&mut self, terminal: &TerminalId, now: Instant) -> Option<StallChange> {
        let watch = self.watches.remove(terminal)?;
        watch.stalled.then_some(StallChange::Left {
            quiet_secs: now.saturating_duration_since(watch.changed_at).as_secs(),
        })
    }

    pub(crate) fn stalled_secs(&self, terminal: &TerminalId, now: Instant) -> Option<u64> {
        let watch = self.watches.get(terminal).filter(|watch| watch.stalled)?;
        Some(now.saturating_duration_since(watch.changed_at).as_secs())
    }

    pub(crate) fn watched(&self) -> Vec<TerminalId> {
        self.watches.keys().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIMIT: Option<Duration> = Some(Duration::from_secs(600));

    #[test]
    fn a_working_agent_with_an_unchanged_screen_stalls_at_the_limit_and_only_once() {
        let terminal = TerminalId::alloc();
        let start = Instant::now();
        let mut table = StallTable::default();

        assert_eq!(table.observe(&terminal, true, 7, start, LIMIT), None);
        let before = start + Duration::from_secs(599);
        assert_eq!(table.observe(&terminal, true, 7, before, LIMIT), None);
        assert_eq!(table.stalled_secs(&terminal, before), None);

        let at = start + Duration::from_secs(600);
        assert_eq!(
            table.observe(&terminal, true, 7, at, LIMIT),
            Some(StallChange::Entered { quiet_secs: 600 })
        );
        let later = start + Duration::from_secs(700);
        assert_eq!(table.observe(&terminal, true, 7, later, LIMIT), None);
        assert_eq!(table.stalled_secs(&terminal, later), Some(700));
    }

    #[test]
    fn a_screen_change_ends_the_stall_and_starts_the_count_again() {
        let terminal = TerminalId::alloc();
        let start = Instant::now();
        let mut table = StallTable::default();
        table.observe(&terminal, true, 1, start, LIMIT);
        table.observe(&terminal, true, 1, start + Duration::from_secs(600), LIMIT);

        let changed = start + Duration::from_secs(650);
        assert_eq!(
            table.observe(&terminal, true, 2, changed, LIMIT),
            Some(StallChange::Left { quiet_secs: 650 })
        );
        assert_eq!(table.stalled_secs(&terminal, changed), None);
        assert_eq!(
            table.observe(
                &terminal,
                true,
                2,
                changed + Duration::from_secs(599),
                LIMIT
            ),
            None
        );
    }

    #[test]
    fn an_agent_that_stops_working_is_not_stalled() {
        let terminal = TerminalId::alloc();
        let start = Instant::now();
        let mut table = StallTable::default();
        table.observe(&terminal, true, 1, start, LIMIT);
        table.observe(&terminal, true, 1, start + Duration::from_secs(600), LIMIT);

        let idle = start + Duration::from_secs(601);
        assert_eq!(
            table.observe(&terminal, false, 1, idle, LIMIT),
            Some(StallChange::Left { quiet_secs: 601 })
        );
        assert_eq!(table.observe(&terminal, false, 1, idle, LIMIT), None);
    }

    #[test]
    fn a_screen_that_keeps_changing_never_stalls() {
        let terminal = TerminalId::alloc();
        let start = Instant::now();
        let mut table = StallTable::default();
        for step in 0..20u64 {
            let now = start + Duration::from_secs(step * 500);
            assert_eq!(table.observe(&terminal, true, step, now, LIMIT), None);
        }
    }

    #[test]
    fn without_a_limit_nothing_stalls() {
        let terminal = TerminalId::alloc();
        let start = Instant::now();
        let mut table = StallTable::default();
        table.observe(&terminal, true, 1, start, None);
        assert_eq!(
            table.observe(&terminal, true, 1, start + Duration::from_secs(9000), None),
            None
        );
    }
}
