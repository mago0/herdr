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
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .replace(|c: char| c.is_control() || c == '|', " ")
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
}

impl Blocker {
    pub(crate) fn from_screen(screen: &str, seq: u64) -> Self {
        let lines = blocker_lines(screen);
        Self {
            text: blocker_text(&lines),
            lines,
            seq: Some(seq),
        }
    }

    pub(crate) fn from_message(message: &str) -> Self {
        let text = one_line(message);
        Self {
            lines: Vec::new(),
            text: (!text.is_empty()).then_some(text),
            seq: None,
        }
    }

    /// Names the dialog on screen. None when the lines did not come from the screen.
    pub(crate) fn dialog(&self) -> Option<String> {
        (self.seq.is_some() && !self.lines.is_empty()).then(|| dialog_id(&self.lines))
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

/// A short name for the lines of a dialog. Two screens with the same lines have the same name,
/// so a caller can say which dialog an answer is for.
pub(crate) fn dialog_id(lines: &[String]) -> String {
    // FNV-1a, 64 bit.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in lines.join("\n").bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
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
        let id = dialog_id(&lines);
        assert_eq!(id.len(), 16);
        assert_eq!(id, dialog_id(&blocker_lines(screen)));
        assert_ne!(
            id,
            dialog_id(&blocker_lines(
                "old\nPick a / b?\n\u{276f} 1. Tea\n2. Milk\n"
            ))
        );
        // The cursor mark is part of the dialog: a moved cursor is another screen.
        assert_ne!(
            id,
            dialog_id(&blocker_lines(
                "old\nPick a / b?\n1. Tea\n\u{276f} 2. Coffee\n"
            ))
        );
        assert!(blocker_lines("  \n───\n").is_empty());
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
