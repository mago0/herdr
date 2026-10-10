//! Prompts that wait for an agent pane to become safe to type into.

use std::collections::VecDeque;

use crate::terminal::TerminalId;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Delivery {
    pub(crate) id: String,
    pub(crate) terminal: TerminalId,
    pub(crate) text: String,
    pub(crate) source: Option<String>,
    pub(crate) queued_at_ms: u64,
    pub(crate) expires_at_ms: Option<u64>,
}

/// What the server knows about a pane when it decides to type a delivery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PaneReadiness {
    /// A known agent is the foreground process and is past its launch.
    pub(crate) agent_ready: bool,
    pub(crate) blocked: bool,
    pub(crate) focused: bool,
    /// A person can type into the pane only while a client is attached.
    pub(crate) client_attached: bool,
}

impl PaneReadiness {
    pub(crate) fn safe(self) -> bool {
        self.agent_ready && !self.blocked && !(self.focused && self.client_attached)
    }
}

#[derive(Debug, Default)]
pub(crate) struct DeliveryQueue {
    items: VecDeque<Delivery>,
    next_id: u64,
}

impl DeliveryQueue {
    pub(crate) fn push(
        &mut self,
        terminal: TerminalId,
        text: String,
        source: Option<String>,
        now_ms: u64,
        ttl_ms: Option<u64>,
    ) -> Delivery {
        self.next_id += 1;
        let delivery = Delivery {
            id: format!("dlv_{now_ms:x}_{}", self.next_id),
            terminal,
            text,
            source,
            queued_at_ms: now_ms,
            expires_at_ms: ttl_ms.map(|ttl| now_ms.saturating_add(ttl)),
        };
        self.items.push_back(delivery.clone());
        delivery
    }

    /// Puts back a delivery that was taken and could not be written.
    pub(crate) fn restore_front(&mut self, delivery: Delivery) {
        self.items.push_front(delivery);
    }

    pub(crate) fn restore(&mut self, delivery: Delivery) {
        self.items.push_back(delivery);
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = &Delivery> {
        self.items.iter()
    }

    pub(crate) fn pending_for(&self, terminal: &TerminalId) -> u32 {
        self.items
            .iter()
            .filter(|delivery| &delivery.terminal == terminal)
            .count() as u32
    }

    /// Terminals that have a delivery, in the order of their oldest one.
    pub(crate) fn terminals(&self) -> Vec<TerminalId> {
        let mut seen = Vec::new();
        for delivery in &self.items {
            if !seen.contains(&delivery.terminal) {
                seen.push(delivery.terminal.clone());
            }
        }
        seen
    }

    /// Takes the oldest delivery of a terminal.
    pub(crate) fn take_next(&mut self, terminal: &TerminalId) -> Option<Delivery> {
        let index = self
            .items
            .iter()
            .position(|delivery| &delivery.terminal == terminal)?;
        self.items.remove(index)
    }

    pub(crate) fn cancel(&mut self, id: &str) -> Option<Delivery> {
        let index = self.items.iter().position(|delivery| delivery.id == id)?;
        self.items.remove(index)
    }

    pub(crate) fn drop_terminal(&mut self, terminal: &TerminalId) -> Vec<Delivery> {
        let (dropped, kept): (Vec<_>, Vec<_>) = std::mem::take(&mut self.items)
            .into_iter()
            .partition(|delivery| &delivery.terminal == terminal);
        self.items = kept.into();
        dropped
    }

    pub(crate) fn expire(&mut self, now_ms: u64) -> Vec<Delivery> {
        let (expired, kept): (Vec<_>, Vec<_>) = std::mem::take(&mut self.items)
            .into_iter()
            .partition(|delivery| delivery.expires_at_ms.is_some_and(|at| now_ms >= at));
        self.items = kept.into();
        expired
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ready() -> PaneReadiness {
        PaneReadiness {
            agent_ready: true,
            blocked: false,
            focused: false,
            client_attached: true,
        }
    }

    #[test]
    fn a_ready_unfocused_pane_is_safe() {
        assert!(ready().safe());
    }

    #[test]
    fn a_focused_pane_is_not_safe_while_a_client_is_attached() {
        let focused = PaneReadiness {
            focused: true,
            ..ready()
        };
        assert!(!focused.safe());
        assert!(PaneReadiness {
            client_attached: false,
            ..focused
        }
        .safe());
    }

    #[test]
    fn a_blocked_agent_or_a_pane_without_a_ready_agent_is_not_safe() {
        assert!(!PaneReadiness {
            blocked: true,
            ..ready()
        }
        .safe());
        assert!(!PaneReadiness {
            agent_ready: false,
            ..ready()
        }
        .safe());
    }

    #[test]
    fn deliveries_leave_in_the_order_they_came_for_each_terminal() {
        let (a, b) = (TerminalId::alloc(), TerminalId::alloc());
        let mut queue = DeliveryQueue::default();
        queue.push(a.clone(), "a1".into(), None, 1, None);
        queue.push(b.clone(), "b1".into(), None, 2, None);
        queue.push(a.clone(), "a2".into(), None, 3, None);

        assert_eq!(queue.terminals(), vec![a.clone(), b.clone()]);
        assert_eq!(queue.pending_for(&a), 2);
        assert_eq!(queue.take_next(&a).map(|d| d.text), Some("a1".into()));
        assert_eq!(queue.take_next(&a).map(|d| d.text), Some("a2".into()));
        assert_eq!(queue.take_next(&a), None);
        assert_eq!(queue.pending_for(&b), 1);
    }

    #[test]
    fn a_delivery_put_back_is_the_next_one_taken() {
        let a = TerminalId::alloc();
        let mut queue = DeliveryQueue::default();
        queue.push(a.clone(), "first".into(), None, 1, None);
        queue.push(a.clone(), "second".into(), None, 2, None);
        let first = queue.take_next(&a).unwrap();
        queue.restore_front(first);
        assert_eq!(queue.take_next(&a).map(|d| d.text), Some("first".into()));
    }

    #[test]
    fn a_delivery_past_its_time_limit_is_dropped_and_others_stay() {
        let a = TerminalId::alloc();
        let mut queue = DeliveryQueue::default();
        queue.push(a.clone(), "short".into(), None, 100, Some(50));
        queue.push(a.clone(), "forever".into(), None, 100, None);
        assert!(queue.expire(149).is_empty());
        let expired = queue.expire(150);
        assert_eq!(expired.len(), 1);
        assert_eq!(expired[0].text, "short");
        assert_eq!(queue.pending_for(&a), 1);
    }

    #[test]
    fn closing_a_terminal_drops_only_its_deliveries() {
        let (a, b) = (TerminalId::alloc(), TerminalId::alloc());
        let mut queue = DeliveryQueue::default();
        queue.push(a.clone(), "a".into(), None, 1, None);
        queue.push(b.clone(), "b".into(), None, 1, None);
        assert_eq!(queue.drop_terminal(&a).len(), 1);
        assert_eq!(queue.pending_for(&a), 0);
        assert_eq!(queue.pending_for(&b), 1);
    }

    #[test]
    fn cancel_removes_one_delivery_by_id() {
        let a = TerminalId::alloc();
        let mut queue = DeliveryQueue::default();
        let first = queue.push(a.clone(), "first".into(), None, 1, None);
        queue.push(a.clone(), "second".into(), None, 1, None);
        assert!(queue.cancel(&first.id).is_some());
        assert!(queue.cancel(&first.id).is_none());
        assert_eq!(queue.take_next(&a).map(|d| d.text), Some("second".into()));
    }
}
