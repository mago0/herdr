//! Which pane supervises which, and what the server reports to the supervisor.

use std::collections::HashMap;

use crate::api::schema::SuperviseReport;
use crate::terminal::TerminalId;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Supervision {
    pub(crate) supervisor: TerminalId,
    pub(crate) report: Vec<SuperviseReport>,
}

impl Supervision {
    pub(crate) fn reports(&self, kind: SuperviseReport) -> bool {
        self.report.contains(&kind)
    }
}

/// True when making `supervisor` supervise `pane` would close a loop.
pub(crate) fn would_cycle(
    links: &HashMap<TerminalId, Supervision>,
    pane: &TerminalId,
    supervisor: &TerminalId,
) -> bool {
    let mut current = supervisor;
    for _ in 0..=links.len() {
        if current == pane {
            return true;
        }
        match links.get(current) {
            Some(link) => current = &link.supervisor,
            None => return false,
        }
    }
    true
}

/// The first pane that `live` accepts on the chain that starts at `supervisor` and goes up
/// through the supervisor of each pane. A pane whose supervisor is gone moves to it.
pub(crate) fn nearest_live_supervisor(
    links: &HashMap<TerminalId, Supervision>,
    supervisor: &TerminalId,
    live: impl Fn(&TerminalId) -> bool,
) -> Option<TerminalId> {
    let mut current = supervisor;
    for _ in 0..=links.len() {
        if live(current) {
            return Some(current.clone());
        }
        current = &links.get(current)?.supervisor;
    }
    None
}

const MAX_DETAIL_CHARS: usize = 300;

/// The line typed into the supervisor pane. Fields are separated by `|`.
pub(crate) fn report_line(kind: &str, name: &str, pane_id: &str, detail: Option<&str>) -> String {
    let mut line = format!("HERDR|{kind}|{}|{pane_id}", one_line(name));
    // The caller bounds the detail; a blocker keeps its end, where the answers are.
    if let Some(detail) = detail.map(clean).filter(|detail| !detail.is_empty()) {
        line.push('|');
        line.push_str(&detail);
    }
    line
}

/// Text with its white space collapsed and no control character or field separator.
fn clean(text: &str) -> String {
    let spaced = text
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .replace(|c: char| c.is_control() || c == '|', " ");
    // No control character is left, so this drops only the characters that hide or reorder text.
    super::deliveries::sanitize(&spaced)
}

/// Screen text as one line that cannot be read as a second field or a second prompt.
pub(crate) fn one_line(text: &str) -> String {
    let collapsed = clean(text);
    let mut line: String = collapsed.chars().take(MAX_DETAIL_CHARS).collect();
    if collapsed.chars().count() > MAX_DETAIL_CHARS {
        line.push_str("...");
    }
    line
}

const BLOCKER_LINES: usize = 8;
const MAX_BLOCKER_CHARS: usize = 400;
const DIALOG_LINES: usize = 16;
const MAX_DIALOG_LINE_CHARS: usize = 160;

/// What a blocked agent shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Blocker {
    /// One line for a report and for a person to read. None when the screen has no text.
    pub(crate) text: Option<String>,
    /// The last lines of the screen that have text, one entry for each line.
    pub(crate) lines: Vec<String>,
    /// The screen content counter the lines were read at. None for text an agent hook gave.
    pub(crate) seq: Option<u64>,
    /// Which wait of the agent this is, by the server's count of waits.
    pub(crate) block: u64,
    /// A hash of every line of the screen that has text.
    screen: u64,
}

impl Blocker {
    pub(crate) fn from_screen(screen: &str, seq: u64, block: u64) -> Self {
        let lines = blocker_lines(screen);
        Self {
            text: blocker_text(&lines),
            lines,
            seq: Some(seq),
            block,
            screen: screen_hash(screen),
        }
    }

    pub(crate) fn from_message(message: &str) -> Self {
        let text = one_line(message);
        Self {
            lines: Vec::new(),
            text: (!text.is_empty()).then_some(text),
            seq: None,
            block: 0,
            screen: 0,
        }
    }

    /// Names the dialog on screen. None when the lines did not come from the screen. The name
    /// changes when any text on the screen changes and when the agent waits again, so a dialog
    /// that looks like an earlier one has another name.
    pub(crate) fn dialog(&self) -> Option<String> {
        (self.seq.is_some() && !self.lines.is_empty())
            .then(|| format!("{:016x}", fnv1a(self.screen, &self.block.to_le_bytes())))
    }
}

/// The last lines of a screen that have text, each as one clean line.
pub(crate) fn blocker_lines(screen: &str) -> Vec<String> {
    let mut lines: Vec<String> = screen
        .lines()
        .map(str::trim)
        .filter(|line| line.chars().any(char::is_alphanumeric))
        .rev()
        .take(DIALOG_LINES)
        .map(|line| clean(line).chars().take(MAX_DIALOG_LINE_CHARS).collect())
        .collect();
    lines.reverse();
    lines
}

/// The lines as one line. The answers of a dialog are at its end, so text that is too long loses
/// its start.
pub(crate) fn blocker_text(lines: &[String]) -> Option<String> {
    let tail = &lines[lines.len().saturating_sub(BLOCKER_LINES)..];
    let text = tail.join(" / ");
    let count = text.chars().count();
    if count == 0 {
        return None;
    }
    if count <= MAX_BLOCKER_CHARS {
        return Some(text);
    }
    let tail: String = text.chars().skip(count - MAX_BLOCKER_CHARS).collect();
    Some(format!("...{tail}"))
}

/// What a blocked agent shows, as one line.
#[cfg(test)]
pub(crate) fn blocker_from_screen(screen: &str) -> Option<String> {
    blocker_text(&blocker_lines(screen))
}

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;

/// FNV-1a, 64 bit, continued from `hash`.
fn fnv1a(mut hash: u64, bytes: &[u8]) -> u64 {
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// A hash of every line of a screen that has text. White space and empty lines do not count.
fn screen_hash(screen: &str) -> u64 {
    screen
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .fold(FNV_OFFSET, |hash, line| {
            fnv1a(fnv1a(hash, clean(line).as_bytes()), b"\n")
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn link(supervisor: &TerminalId) -> Supervision {
        Supervision {
            supervisor: supervisor.clone(),
            report: Vec::new(),
        }
    }

    #[test]
    fn a_pane_cannot_supervise_itself_or_one_of_its_supervisors() {
        let (a, b, c) = (
            TerminalId::alloc(),
            TerminalId::alloc(),
            TerminalId::alloc(),
        );
        let mut links = HashMap::new();
        links.insert(b.clone(), link(&a));
        links.insert(c.clone(), link(&b));

        assert!(would_cycle(&links, &a, &a));
        assert!(would_cycle(&links, &a, &c));
        assert!(would_cycle(&links, &b, &c));
        assert!(!would_cycle(&links, &c, &a));
    }

    #[test]
    fn the_nearest_live_supervisor_is_found_above_the_panes_that_are_gone() {
        let (a, b, c, d) = (
            TerminalId::alloc(),
            TerminalId::alloc(),
            TerminalId::alloc(),
            TerminalId::alloc(),
        );
        let mut links = HashMap::new();
        links.insert(b.clone(), link(&a));
        links.insert(c.clone(), link(&b));
        links.insert(d.clone(), link(&c));

        let gone = [b.clone(), c.clone()];
        let live = |pane: &TerminalId| !gone.contains(pane);
        assert_eq!(nearest_live_supervisor(&links, &c, live), Some(a.clone()));
        assert_eq!(nearest_live_supervisor(&links, &b, live), Some(a.clone()));
        assert_eq!(nearest_live_supervisor(&links, &a, live), Some(a.clone()));
        assert_eq!(nearest_live_supervisor(&links, &c, |pane| pane == &d), None);

        // A loop in the links ends the walk.
        links.insert(a.clone(), link(&c));
        assert_eq!(nearest_live_supervisor(&links, &c, |_| false), None);
    }

    #[test]
    fn a_report_line_has_no_field_separator_or_line_break_inside_a_field() {
        let line = report_line(
            "blocked",
            "worker|one",
            "w1:p2",
            Some("Allow?\n  1. Yes | 2. No\x1b[0m"),
        );
        assert_eq!(
            line,
            "HERDR|blocked|worker one|w1:p2|Allow? 1. Yes   2. No [0m"
        );
        assert_eq!(line.lines().count(), 1);
        assert_eq!(line.matches('|').count(), 4);
    }

    #[test]
    fn a_report_line_without_detail_ends_at_the_pane_id() {
        assert_eq!(
            report_line("exited", "worker", "w1:p2", None),
            "HERDR|exited|worker|w1:p2"
        );
        assert_eq!(
            report_line("exited", "worker", "w1:p2", Some("  \n")),
            "HERDR|exited|worker|w1:p2"
        );
    }

    #[test]
    fn long_detail_is_cut() {
        let line = one_line(&"x".repeat(1000));
        assert_eq!(line.chars().count(), MAX_DETAIL_CHARS + 3);
        assert!(line.ends_with("..."));
    }

    #[test]
    fn the_blocker_is_the_last_lines_of_the_screen_that_have_text() {
        let screen = "old output\n\n".repeat(20)
            + "Do you want to run this command?\n  rm -rf build\n ❯ 1. Yes\n   2. No\n\n─────\n";
        assert_eq!(
            blocker_from_screen(&screen).as_deref(),
            Some(
                "old output / old output / old output / old output / \
                 Do you want to run this command? / rm -rf build / ❯ 1. Yes / 2. No"
            )
        );
        assert_eq!(blocker_from_screen("  \n───\n"), None);
    }

    #[test]
    fn the_lines_of_a_blocker_keep_their_own_text_and_name_one_dialog() {
        let screen = "old\n\nPick a / b?\n  \u{276f} 1. Tea\n    2. Coffee\n\n────\n";
        let lines = blocker_lines(screen);
        assert_eq!(
            lines,
            vec!["old", "Pick a / b?", "\u{276f} 1. Tea", "2. Coffee"]
        );
        assert_eq!(
            blocker_text(&lines).as_deref(),
            Some("old / Pick a / b? / \u{276f} 1. Tea / 2. Coffee")
        );
        assert!(blocker_lines("  \n───\n").is_empty());
    }

    #[test]
    fn a_dialog_name_follows_the_whole_screen_and_the_count_of_waits() {
        let name = |screen: &str, block| Blocker::from_screen(screen, 1, block).dialog().unwrap();
        let screen = "old\n\nPick a / b?\n  \u{276f} 1. Tea\n    2. Coffee\n";
        let id = name(screen, 1);
        assert_eq!(id.len(), 16);
        // White space and the content counter are not part of the name.
        assert_eq!(
            Some(id.clone()),
            Blocker::from_screen("old\nPick a / b?\n\u{276f} 1. Tea\n2. Coffee", 9, 1).dialog()
        );
        assert_ne!(id, name("old\nPick a / b?\n\u{276f} 1. Tea\n2. Milk\n", 1));
        // The cursor mark is part of the dialog: a moved cursor is another screen.
        assert_ne!(
            id,
            name("old\nPick a / b?\n1. Tea\n\u{276f} 2. Coffee\n", 1)
        );
        // The same screen at a later wait is another dialog.
        assert_ne!(id, name(screen, 2));
        // A change above the lines that are kept for a person still changes the name.
        let long = |first: &str| format!("{first}\n{}1. Yes\n2. No\n", "same line\n".repeat(40));
        assert_eq!(
            Blocker::from_screen(&long("rm -rf a"), 1, 1).lines,
            Blocker::from_screen(&long("rm -rf b"), 1, 1).lines
        );
        assert_ne!(name(&long("rm -rf a"), 1), name(&long("rm -rf b"), 1));
        assert_eq!(Blocker::from_message("needs you").dialog(), None);
    }

    #[test]
    fn blocker_lines_keep_no_character_that_hides_text() {
        assert_eq!(
            blocker_lines("Run\u{202e} this\u{e0041}?\n1. Yes\u{2028}\n"),
            vec!["Run this?", "1. Yes"]
        );
    }

    #[test]
    fn a_long_blocker_keeps_its_end_where_the_answers_are() {
        let long_line = "why ".repeat(30);
        let screen = format!(
            "{}1. Yes, I trust this folder\n2. No, exit\n",
            format!("{long_line}\n").repeat(6)
        );
        let blocker = blocker_from_screen(&screen).unwrap();
        assert!(blocker.starts_with("..."));
        assert!(blocker.ends_with("why / 1. Yes, I trust this folder / 2. No, exit"));
        assert_eq!(blocker.chars().count(), MAX_BLOCKER_CHARS + 3);
    }
}
