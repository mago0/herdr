#[derive(Clone)]
pub struct EventHub {
    inner: std::sync::Arc<std::sync::Mutex<EventHubState>>,
    /// Identifies this run of the hub. Sequence numbers start again in a new run.
    epoch: std::sync::Arc<str>,
}

impl Default for EventHub {
    fn default() -> Self {
        static NEXT_HUB: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0);
        let hub = NEXT_HUB.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Self {
            inner: Default::default(),
            epoch: format!("{nanos:x}-{:x}-{hub:x}", std::process::id()).into(),
        }
    }
}

#[derive(Default)]
struct EventHubState {
    next_sequence: u64,
    events: Vec<(u64, crate::api::schema::EventEnvelope)>,
}

/// One answer to a caller that reads history with a cursor.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct EventHistory {
    pub(crate) latest: u64,
    pub(crate) lost: bool,
    pub(crate) events: Vec<(u64, crate::api::schema::EventEnvelope)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum EventHistoryError {
    Lost,
    Unavailable,
}

impl EventHub {
    // Tests that fill the history over a socket keep the small size.
    const MAX_EVENTS: usize = if cfg!(test) { 512 } else { 4096 };

    pub fn push(&self, event: crate::api::schema::EventEnvelope) {
        let Ok(mut state) = self.inner.lock() else {
            return;
        };
        state.next_sequence += 1;
        let sequence = state.next_sequence;
        state.events.push((sequence, event));
        let overflow = state.events.len().saturating_sub(Self::MAX_EVENTS);
        if overflow > 0 {
            state.events.drain(0..overflow);
        }
    }

    pub fn events_after(&self, sequence: u64) -> Vec<(u64, crate::api::schema::EventEnvelope)> {
        let Ok(state) = self.inner.lock() else {
            return Vec::new();
        };
        state
            .events
            .iter()
            .filter(|(event_sequence, _)| *event_sequence > sequence)
            .cloned()
            .collect()
    }

    pub(super) fn events_after_checked(
        &self,
        sequence: u64,
    ) -> Result<Vec<(u64, crate::api::schema::EventEnvelope)>, EventHistoryError> {
        let state = self
            .inner
            .lock()
            .map_err(|_| EventHistoryError::Unavailable)?;
        if state
            .events
            .first()
            .is_some_and(|(first, _)| sequence < first.saturating_sub(1))
        {
            return Err(EventHistoryError::Lost);
        }
        Ok(state
            .events
            .iter()
            .filter(|(event_sequence, _)| *event_sequence > sequence)
            .cloned()
            .collect())
    }

    pub(crate) fn epoch(&self) -> &str {
        &self.epoch
    }

    /// Events after a cursor, oldest first. `kinds` keeps only those dotted names; `limit` caps
    /// the count, and a caller that gets a full page reads again from its last sequence.
    pub(crate) fn read(&self, after: Option<u64>, kinds: &[String], limit: usize) -> EventHistory {
        let Ok(state) = self.inner.lock() else {
            return EventHistory {
                latest: 0,
                lost: true,
                events: Vec::new(),
            };
        };
        let latest = state.next_sequence;
        let Some(after) = after else {
            return EventHistory {
                latest,
                lost: false,
                events: Vec::new(),
            };
        };
        let oldest_known = state
            .events
            .first()
            .map_or(latest, |(first, _)| first.saturating_sub(1));
        let events = state
            .events
            .iter()
            .filter(|(sequence, _)| *sequence > after)
            .filter(|(_, event)| {
                kinds.is_empty() || kinds.iter().any(|kind| kind == event.event.dot_name())
            })
            .take(limit)
            .cloned()
            .collect();
        EventHistory {
            latest,
            lost: after < oldest_known || after > latest,
            events,
        }
    }

    pub fn current_sequence(&self) -> u64 {
        let Ok(state) = self.inner.lock() else {
            return 0;
        };
        state.next_sequence
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::schema::{EventData, EventEnvelope, EventKind};

    fn event() -> EventEnvelope {
        EventEnvelope {
            event: EventKind::WorkspaceFocused,
            data: EventData::WorkspaceFocused {
                workspace_id: "workspace_1".into(),
            },
        }
    }

    fn pane_closed(pane: &str) -> EventEnvelope {
        EventEnvelope {
            event: EventKind::PaneClosed,
            data: EventData::PaneClosed {
                pane_id: pane.into(),
                workspace_id: "w1".into(),
            },
        }
    }

    #[test]
    fn a_read_without_a_cursor_returns_the_latest_sequence_and_no_events() {
        let hub = EventHub::default();
        hub.push(event());
        hub.push(event());
        let history = hub.read(None, &[], 100);
        assert_eq!((history.latest, history.lost), (2, false));
        assert!(history.events.is_empty());
    }

    #[test]
    fn a_read_returns_the_events_after_the_cursor_in_order() {
        let hub = EventHub::default();
        hub.push(event());
        hub.push(pane_closed("w1:p1"));
        hub.push(pane_closed("w1:p2"));
        let history = hub.read(Some(1), &[], 100);
        assert!(!history.lost);
        assert_eq!(history.latest, 3);
        assert_eq!(
            history.events,
            vec![(2, pane_closed("w1:p1")), (3, pane_closed("w1:p2"))]
        );
        assert!(hub.read(Some(3), &[], 100).events.is_empty());
    }

    #[test]
    fn a_read_keeps_only_the_named_kinds_and_stops_at_the_limit() {
        let hub = EventHub::default();
        hub.push(event());
        hub.push(pane_closed("w1:p1"));
        hub.push(event());
        hub.push(pane_closed("w1:p2"));
        let kinds = vec!["pane.closed".to_string()];
        let all = hub.read(Some(0), &kinds, 100);
        assert_eq!(
            all.events.iter().map(|(seq, _)| *seq).collect::<Vec<_>>(),
            vec![2, 4]
        );
        let first = hub.read(Some(0), &kinds, 1);
        assert_eq!(first.events.len(), 1);
        assert_eq!(first.events[0].0, 2);
    }

    #[test]
    fn a_cursor_older_than_the_history_or_from_another_run_is_reported_lost() {
        let hub = EventHub::default();
        for _ in 0..EventHub::MAX_EVENTS + 10 {
            hub.push(event());
        }
        assert!(hub.read(Some(5), &[], 10).lost);
        assert!(!hub.read(Some(10), &[], 10).lost);
        assert!(hub.read(Some(999_999), &[], 10).lost);
    }

    #[test]
    fn each_hub_has_its_own_epoch() {
        assert_ne!(EventHub::default().epoch(), EventHub::default().epoch());
    }

    #[test]
    fn checked_history_distinguishes_retained_boundary_from_lost_events() {
        let hub = EventHub::default();
        assert!(hub.events_after_checked(0).unwrap().is_empty());
        for _ in 0..EventHub::MAX_EVENTS {
            hub.push(event());
        }
        assert_eq!(
            hub.events_after_checked(0).unwrap().len(),
            EventHub::MAX_EVENTS
        );
        hub.push(event());
        assert_eq!(hub.events_after_checked(0), Err(EventHistoryError::Lost));
        let retained = hub.events_after_checked(1).unwrap();
        assert_eq!(retained.len(), EventHub::MAX_EVENTS);
        assert_eq!(retained.first().unwrap().0, 2);
        assert_eq!(retained.last().unwrap().0, hub.current_sequence());
        assert!(hub
            .events_after_checked(hub.current_sequence())
            .unwrap()
            .is_empty());
    }

    #[test]
    fn checked_history_reports_unavailable_instead_of_empty_after_poison() {
        let hub = EventHub::default();
        assert!(std::panic::catch_unwind(|| {
            let _guard = hub.inner.lock().unwrap();
            panic!("poison the test event history");
        })
        .is_err());
        assert_eq!(
            hub.events_after_checked(0),
            Err(EventHistoryError::Unavailable)
        );
    }
}
