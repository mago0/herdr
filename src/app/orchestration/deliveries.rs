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
    /// A client is attached to this terminal alone, so a person has it in front of them.
    pub(crate) attached_directly: bool,
}

impl PaneReadiness {
    pub(crate) fn safe(self) -> bool {
        self.hold().is_none()
    }

    /// Why a delivery waits, by a short name a caller can show. None when the pane is safe.
    pub(crate) fn hold(self) -> Option<&'static str> {
        if !self.agent_ready {
            Some("agent_not_ready")
        } else if !self.waiting_for_input {
            Some("agent_busy")
        } else if self.unsent_input {
            Some("unsent_input")
        } else if (self.focused && self.client_attached) || self.attached_directly {
            Some("focused")
        } else {
            None
        }
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
            | '\u{2028}'
            | '\u{2029}'
            | '\u{061c}'
            | '\u{00ad}'
            | '\u{180e}'
            | '\u{e0000}'..='\u{e007f}'
            | '\u{feff}' => false,
            _ => true,
        })
        .collect()
}

/// What one key from a person does to the text in a pane's input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DraftKey {
    /// Adds this many characters.
    Text(usize),
    /// Erases one character.
    Erase,
    /// Sends or drops the whole text.
    Clear,
}

/// Stands for text of a length the server cannot know, such as a prompt recalled from history.
const UNKNOWN_LENGTH: usize = 1 << 20;

/// The keys of a batch that change the text in the input, in order. A key that moves the cursor,
/// a release, and Enter with a modifier (a line break in an agent's input) are not in the result.
pub(crate) fn draft_keys(events: &[crate::protocol::ClientPaneInputEvent]) -> Vec<DraftKey> {
    use crate::protocol::{ClientKeyCode, ClientKeyKind, ClientPaneInputEvent};
    const CTRL: u8 = 0b10;
    const ALT: u8 = 0b100;
    events
        .iter()
        .filter_map(|event| match event {
            ClientPaneInputEvent::Key {
                kind: ClientKeyKind::Release,
                ..
            } => None,
            ClientPaneInputEvent::Key {
                code: ClientKeyCode::Enter,
                modifiers: 0,
                ..
            } => Some(DraftKey::Clear),
            ClientPaneInputEvent::Key {
                code: ClientKeyCode::Char('c' | 'u'),
                modifiers,
                ..
            } if modifiers & CTRL != 0 => Some(DraftKey::Clear),
            ClientPaneInputEvent::Key {
                code: ClientKeyCode::Backspace,
                modifiers: 0,
                ..
            } => Some(DraftKey::Erase),
            ClientPaneInputEvent::Key {
                code: ClientKeyCode::Up,
                modifiers: 0,
                ..
            } => Some(DraftKey::Text(UNKNOWN_LENGTH)),
            ClientPaneInputEvent::Key {
                code: ClientKeyCode::Char(_),
                modifiers,
                ..
            } if modifiers & (CTRL | ALT) == 0 => Some(DraftKey::Text(1)),
            ClientPaneInputEvent::Key {
                generated_text: Some(text),
                ..
            } if !text.is_empty() => Some(DraftKey::Text(text.chars().count())),
            ClientPaneInputEvent::TextCommit(text) | ClientPaneInputEvent::Paste(text)
                if !text.is_empty() =>
            {
                Some(DraftKey::Text(text.chars().count()))
            }
            _ => None,
        })
        .collect()
}

/// The same reading for the raw bytes of a client that is attached to one terminal.
pub(crate) fn draft_keys_from_bytes(data: &[u8]) -> Vec<DraftKey> {
    if data.contains(&0x1b) {
        // An escape sequence is a paste, the Up key, or a key that leaves the text as it is.
        return if data.starts_with(b"\x1b[200~") || matches!(data, b"\x1b[A" | b"\x1bOA") {
            vec![DraftKey::Text(UNKNOWN_LENGTH)]
        } else {
            Vec::new()
        };
    }
    String::from_utf8_lossy(data)
        .chars()
        .filter_map(|c| match c {
            '\r' | '\u{3}' | '\u{15}' => Some(DraftKey::Clear),
            '\u{7f}' | '\u{8}' => Some(DraftKey::Erase),
            c if c.is_control() => None,
            _ => Some(DraftKey::Text(1)),
        })
        .collect()
}

/// How long text that was typed and not sent holds deliveries after the last key. A person who
/// left text in a pane and went away must not stop that pane's deliveries without end.
pub(crate) const DRAFT_HOLD: std::time::Duration = std::time::Duration::from_secs(600);

#[derive(Debug, Clone, Copy)]
struct Draft {
    chars: usize,
    last_key: std::time::Instant,
}

/// Panes whose input holds text a person typed and did not send, as far as the keys tell.
#[derive(Debug, Default)]
pub(crate) struct DraftTracker {
    drafts: std::collections::HashMap<TerminalId, Draft>,
}

impl DraftTracker {
    /// Applies keys in order. True when the pane went from text to no text.
    pub(crate) fn apply(
        &mut self,
        terminal: &TerminalId,
        keys: &[DraftKey],
        now: std::time::Instant,
    ) -> bool {
        let had = self.drafts.contains_key(terminal);
        let mut chars = self.drafts.get(terminal).map_or(0, |draft| draft.chars);
        let mut typed = false;
        for key in keys {
            match key {
                DraftKey::Text(count) => {
                    chars = chars.saturating_add(*count);
                    typed = true;
                }
                DraftKey::Erase => chars = chars.saturating_sub(1),
                DraftKey::Clear => chars = 0,
            }
        }
        if chars == 0 {
            self.drafts.remove(terminal);
            return had;
        }
        let last_key = match self.drafts.get(terminal) {
            Some(draft) if !typed => draft.last_key,
            _ => now,
        };
        self.drafts
            .insert(terminal.clone(), Draft { chars, last_key });
        false
    }

    pub(crate) fn clear(&mut self, terminal: &TerminalId) -> bool {
        self.drafts.remove(terminal).is_some()
    }

    pub(crate) fn holds(&self, terminal: &TerminalId, now: std::time::Instant) -> bool {
        self.drafts
            .get(terminal)
            .is_some_and(|draft| now.saturating_duration_since(draft.last_key) < DRAFT_HOLD)
    }

    pub(crate) fn retain(&mut self, keep: impl Fn(&TerminalId) -> bool) {
        self.drafts.retain(|terminal, _| keep(terminal));
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

    /// Drops the deliveries of a terminal for which `drop` is true. Gives the count.
    pub(crate) fn remove_where(
        &mut self,
        terminal: &TerminalId,
        drop: impl Fn(&Delivery) -> bool,
    ) -> usize {
        let before = self.items.len();
        self.items
            .retain(|delivery| &delivery.terminal != terminal || !drop(delivery));
        before - self.items.len()
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
            attached_directly: false,
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
        // So are characters a person cannot see and a model can read.
        assert_eq!(
            sanitize("a\u{2028}b\u{2029}c\u{e0041}d\u{ad}e\u{61c}f\u{180e}g"),
            "abcdefg"
        );
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

    fn key(
        code: crate::protocol::ClientKeyCode,
        modifiers: u8,
        kind: crate::protocol::ClientKeyKind,
    ) -> crate::protocol::ClientPaneInputEvent {
        crate::protocol::ClientPaneInputEvent::Key {
            code,
            modifiers,
            kind,
            repeat_count: 1,
            shifted_codepoint: None,
            generated_text: None,
            tracks_release: false,
            physical_key_id: None,
            windows_record: None,
        }
    }

    #[test]
    fn keys_are_read_as_text_an_erase_or_a_clear() {
        use crate::protocol::{
            ClientKeyCode as Code, ClientKeyKind as Kind, ClientPaneInputEvent as Event,
        };
        const SHIFT: u8 = 0b1;
        const CTRL: u8 = 0b10;
        const ALT: u8 = 0b100;
        let press = |code| key(code, 0, Kind::Press);
        assert_eq!(
            draft_keys(&[
                press(Code::Char('h')),
                press(Code::Backspace),
                press(Code::Enter)
            ]),
            vec![DraftKey::Text(1), DraftKey::Erase, DraftKey::Clear]
        );
        assert_eq!(
            draft_keys(&[Event::Paste("text".into()), Event::TextCommit("é".into())]),
            vec![DraftKey::Text(4), DraftKey::Text(1)]
        );
        assert_eq!(
            draft_keys(&[
                key(Code::Char('c'), CTRL, Kind::Press),
                key(Code::Char('u'), CTRL, Kind::Press)
            ]),
            vec![DraftKey::Clear, DraftKey::Clear]
        );
        // Enter with a modifier is a line break in the input, and Ctrl+D erases one character
        // at most: the text stays.
        assert_eq!(
            draft_keys(&[
                key(Code::Enter, SHIFT, Kind::Press),
                key(Code::Enter, ALT, Kind::Press),
                key(Code::Char('d'), CTRL, Kind::Press),
                press(Code::Left),
                key(Code::Char('h'), 0, Kind::Release),
                Event::Paste(String::new()),
            ]),
            Vec::new()
        );
        // A prompt recalled from history is text of a length the server does not know.
        assert!(matches!(
            draft_keys(&[press(Code::Up)])[..],
            [DraftKey::Text(count)] if count > 1000
        ));
    }

    #[test]
    fn raw_bytes_of_an_attached_client_are_read_the_same_way() {
        assert_eq!(
            draft_keys_from_bytes(b"hi\x7f\r"),
            vec![
                DraftKey::Text(1),
                DraftKey::Text(1),
                DraftKey::Erase,
                DraftKey::Clear
            ]
        );
        assert_eq!(draft_keys_from_bytes(b"\x03"), vec![DraftKey::Clear]);
        assert_eq!(draft_keys_from_bytes(b"\x1b[D"), Vec::new());
        assert_eq!(draft_keys_from_bytes(b"\x1b[200~text\x1b[201~").len(), 1);
        assert_eq!(draft_keys_from_bytes(b"\x1b[A").len(), 1);
    }

    #[test]
    fn a_draft_ends_when_its_text_is_erased_sent_or_old() {
        let pane = TerminalId::alloc();
        let start = std::time::Instant::now();
        let mut drafts = DraftTracker::default();

        assert!(!drafts.apply(&pane, &[DraftKey::Text(1)], start));
        assert!(drafts.holds(&pane, start));
        // One character typed and erased leaves no text.
        assert!(drafts.apply(&pane, &[DraftKey::Erase], start));
        assert!(!drafts.holds(&pane, start));
        // An erase with no text is not a change.
        assert!(!drafts.apply(&pane, &[DraftKey::Erase], start));

        drafts.apply(&pane, &[DraftKey::Text(2), DraftKey::Erase], start);
        assert!(drafts.holds(&pane, start));
        assert!(drafts.apply(&pane, &[DraftKey::Clear], start));
        assert!(!drafts.holds(&pane, start));

        // Text that nobody touched for the hold time no longer holds deliveries, and a key that
        // adds no text does not start the time again.
        drafts.apply(&pane, &[DraftKey::Text(3)], start);
        drafts.apply(&pane, &[DraftKey::Erase], start + DRAFT_HOLD);
        assert!(drafts.holds(
            &pane,
            start + DRAFT_HOLD - std::time::Duration::from_secs(1)
        ));
        assert!(!drafts.holds(&pane, start + DRAFT_HOLD));
        drafts.apply(&pane, &[DraftKey::Text(1)], start + DRAFT_HOLD);
        assert!(drafts.holds(&pane, start + DRAFT_HOLD));
    }

    #[test]
    fn a_terminal_with_a_client_attached_to_it_alone_is_not_safe() {
        let attached = PaneReadiness {
            attached_directly: true,
            client_attached: false,
            ..ready()
        };
        assert_eq!(attached.hold(), Some("focused"));
    }

    #[test]
    fn the_reason_a_delivery_waits_has_a_name() {
        assert_eq!(ready().hold(), None);
        assert_eq!(
            PaneReadiness {
                waiting_for_input: false,
                ..ready()
            }
            .hold(),
            Some("agent_busy")
        );
        assert_eq!(
            PaneReadiness {
                unsent_input: true,
                ..ready()
            }
            .hold(),
            Some("unsent_input")
        );
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
