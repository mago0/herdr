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

/// The most deliveries that can wait for one terminal.
pub(crate) const MAX_PENDING_PER_TERMINAL: usize = 64;

/// What the server knows about a pane when it decides to type a delivery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PaneReadiness {
    /// A known agent is the foreground process and is past its launch.
    pub(crate) agent_ready: bool,
    /// The agent is at its prompt. A working agent can open a dialog between the text and the
    /// Enter of a delivery, and a blocked or unknown one may already show one.
    pub(crate) waiting_for_input: bool,
    /// A person typed text into the pane and has not sent or dropped it.
    pub(crate) unsent_input: bool,
    pub(crate) focused: bool,
    /// A person can type into the pane only while a client is attached.
    pub(crate) client_attached: bool,
}

impl PaneReadiness {
    pub(crate) fn safe(self) -> bool {
        self.agent_ready
            && self.waiting_for_input
            && !self.unsent_input
            && !(self.focused && self.client_attached)
    }
}

/// Text with every character removed that a terminal reads as a key or that changes how the
/// text shows: control characters other than line feed and tab, and Unicode format characters.
pub(crate) fn sanitize(text: &str) -> String {
    text.chars()
        .filter(|c| match *c {
            '\n' | '\t' => true,
            c if c.is_control() => false,
            '\u{200b}'..='\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{2069}'
            | '\u{feff}' => false,
            _ => true,
        })
        .collect()
}

/// What a batch of keys from a person says about text in the pane's input: `Some(true)` when it
/// leaves text that is not sent, `Some(false)` when it sends or drops the text, and `None` when it
/// says nothing sure. The last key that says something decides.
pub(crate) fn draft_change(events: &[crate::protocol::ClientPaneInputEvent]) -> Option<bool> {
    use crate::protocol::{ClientKeyCode, ClientKeyKind, ClientPaneInputEvent};
    const CTRL: u8 = 0b10;
    const ALT: u8 = 0b100;
    // The last key that says something decides, so the keys are read from the end.
    events.iter().rev().find_map(|event| match event {
        ClientPaneInputEvent::Key {
            kind: ClientKeyKind::Release,
            ..
        } => None,
        ClientPaneInputEvent::Key {
            code: ClientKeyCode::Enter,
            ..
        } => Some(false),
        ClientPaneInputEvent::Key {
            code: ClientKeyCode::Char('c' | 'u' | 'd'),
            modifiers,
            ..
        } if modifiers & CTRL != 0 => Some(false),
        ClientPaneInputEvent::Key {
            code: ClientKeyCode::Char(_),
            modifiers,
            ..
        } if modifiers & (CTRL | ALT) == 0 => Some(true),
        ClientPaneInputEvent::Key {
            generated_text: Some(text),
            ..
        } if !text.is_empty() => Some(true),
        ClientPaneInputEvent::TextCommit(text) | ClientPaneInputEvent::Paste(text)
            if !text.is_empty() =>
        {
            Some(true)
        }
        _ => None,
    })
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

    pub(crate) fn has_room(&self, terminal: &TerminalId) -> bool {
        (self.pending_for(terminal) as usize) < MAX_PENDING_PER_TERMINAL
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
            waiting_for_input: true,
            unsent_input: false,
            focused: false,
            client_attached: true,
        }
    }

    #[test]
    fn delivery_text_keeps_no_character_that_a_terminal_reads_as_a_key() {
        // The end of a bracketed paste, a carriage return and an interrupt would each act as keys.
        assert_eq!(
            sanitize("go\x1b[201~\rrm -rf /\x03 now\u{9b}A"),
            "go[201~rm -rf / nowA"
        );
        assert_eq!(sanitize("line one\n\tline two"), "line one\n\tline two");
        // Characters that reorder or hide text on screen are dropped too.
        assert_eq!(sanitize("a\u{202e}b\u{200b}c\u{feff}d\u{2066}e"), "abcde");
        assert_eq!(sanitize("\x1b\r\x00"), "");
    }

    #[test]
    fn a_pane_with_text_that_was_typed_and_not_sent_is_not_safe() {
        assert!(!PaneReadiness {
            unsent_input: true,
            ..ready()
        }
        .safe());
    }

    #[test]
    fn only_an_agent_that_waits_for_input_is_safe_to_type_into() {
        assert!(!PaneReadiness {
            waiting_for_input: false,
            ..ready()
        }
        .safe());
    }

    #[test]
    fn typed_text_marks_a_draft_until_it_is_sent_or_dropped() {
        use crate::protocol::{
            ClientKeyCode as Code, ClientKeyKind as Kind, ClientPaneInputEvent as Event,
        };
        let key = |code: Code, modifiers: u8, kind: Kind| Event::Key {
            code,
            modifiers,
            kind,
            repeat_count: 1,
            shifted_codepoint: None,
            generated_text: None,
            tracks_release: false,
            physical_key_id: None,
            windows_record: None,
        };
        const CTRL: u8 = 0b10;
        let press = |code| key(code, 0, Kind::Press);
        assert_eq!(draft_change(&[press(Code::Char('h'))]), Some(true));
        assert_eq!(draft_change(&[Event::Paste("text".into())]), Some(true));
        assert_eq!(draft_change(&[Event::TextCommit("é".into())]), Some(true));
        assert_eq!(
            draft_change(&[press(Code::Char('h')), press(Code::Enter)]),
            Some(false)
        );
        assert_eq!(
            draft_change(&[press(Code::Enter), press(Code::Char('h'))]),
            Some(true)
        );
        assert_eq!(
            draft_change(&[key(Code::Char('c'), CTRL, Kind::Press)]),
            Some(false)
        );
        assert_eq!(
            draft_change(&[key(Code::Char('u'), CTRL, Kind::Press)]),
            Some(false)
        );
        // Keys that move or erase say nothing sure about the text, and a release is not a key.
        assert_eq!(
            draft_change(&[press(Code::Left), press(Code::Backspace)]),
            None
        );
        assert_eq!(
            draft_change(&[key(Code::Char('h'), 0, Kind::Release)]),
            None
        );
        assert_eq!(draft_change(&[Event::Paste(String::new())]), None);
        assert_eq!(draft_change(&[]), None);
    }

    #[test]
    fn a_full_queue_of_one_terminal_takes_no_more() {
        let (a, b) = (TerminalId::alloc(), TerminalId::alloc());
        let mut queue = DeliveryQueue::default();
        for index in 0..MAX_PENDING_PER_TERMINAL {
            assert!(queue.has_room(&a), "{index}");
            queue.push(a.clone(), "x".into(), None, 1, None);
        }
        assert!(!queue.has_room(&a));
        assert!(queue.has_room(&b));
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
    fn a_pane_without_a_ready_agent_is_not_safe() {
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
