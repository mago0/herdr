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

/// What a blocked agent shows: the last lines of its screen that have text. The answers of a
/// dialog are at its end, so text that is too long loses its start.
pub(crate) fn blocker_from_screen(screen: &str) -> Option<String> {
    let mut lines: Vec<&str> = screen
        .lines()
        .map(str::trim)
        .filter(|line| line.chars().any(char::is_alphanumeric))
        .rev()
        .take(BLOCKER_LINES)
        .collect();
    lines.reverse();
    let text = clean(&lines.join(" / "));
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
    fn a_long_blocker_keeps_its_end_where_the_answers_are() {
        let screen = format!(
            "{}\n1. Yes, I trust this folder\n2. No, exit\n",
            "why ".repeat(300)
        );
        let blocker = blocker_from_screen(&screen).unwrap();
        assert!(blocker.starts_with("..."));
        assert!(blocker.ends_with("why / 1. Yes, I trust this folder / 2. No, exit"));
        assert_eq!(blocker.chars().count(), MAX_BLOCKER_CHARS + 3);
    }
}
