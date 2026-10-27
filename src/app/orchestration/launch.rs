//! Agent launches that any tool can rely on: the shell state a launch waits for.

use crate::api::schema::{
    PaneShellState, PaneShellStateInfo, PaneShellStateParams, ResponseResult,
};
use crate::app::api::responses::{encode_error, encode_success};
use crate::app::App;

/// Shells whose prompt reads input through a line editor. The editor turns off line-at-a-time
/// input while it waits, so the tty mode tells startup apart from the prompt.
fn shell_uses_line_editor(name: &str) -> bool {
    let name = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let name = name.trim_start_matches('-').trim_end_matches(".exe");
    matches!(
        name.to_ascii_lowercase().as_str(),
        "bash" | "zsh" | "fish" | "tcsh" | "nu" | "elvish" | "xonsh" | "pwsh" | "powershell"
    )
}

/// The state of a pane whose root shell is the only foreground process.
pub(crate) fn idle_shell_state(shell_name: &str, reads_lines: Option<bool>) -> PaneShellState {
    if shell_uses_line_editor(shell_name) && reads_lines == Some(true) {
        PaneShellState::Starting
    } else {
        PaneShellState::Ready
    }
}

/// True when the root shell leads the foreground process group while other processes run in
/// it. Commands run from startup files do that; a job started at the prompt gets its own group.
#[cfg(unix)]
fn shell_runs_startup_commands(shell_pid: u32) -> bool {
    crate::detect::foreground_job(shell_pid).is_some_and(|job| {
        job.process_group_id == shell_pid
            && job.processes.iter().any(|process| {
                process.pid == shell_pid
                    && (crate::platform::is_pane_shell_process_name(&process.name)
                        || process
                            .argv
                            .as_ref()
                            .and_then(|argv| argv.first())
                            .is_some_and(|argv0| {
                                crate::platform::is_pane_shell_process_name(argv0)
                            }))
            })
    })
}

#[cfg(not(unix))]
fn shell_runs_startup_commands(_shell_pid: u32) -> bool {
    false
}

impl App {
    pub(crate) fn pane_shell_state(&self, pane_id: &str) -> Option<PaneShellStateInfo> {
        let (ws_idx, pane) = self.parse_current_public_pane_id(pane_id)?;
        let terminal_id = self.state.workspaces.get(ws_idx)?.terminal_id(pane)?;
        let terminal = self.state.terminals.get(terminal_id)?;
        let runtime = self.terminal_runtimes.get(terminal_id)?;
        let state = if terminal.is_agent_terminal() || terminal.managed_agent_kind().is_some() {
            PaneShellState::Busy
        } else if let Some(shell_pid) = runtime.child_pid() {
            if let Some(shell) = crate::platform::available_pane_shell(shell_pid) {
                idle_shell_state(&shell, runtime.tty_reads_lines())
            } else if shell_runs_startup_commands(shell_pid) {
                PaneShellState::Starting
            } else {
                PaneShellState::Busy
            }
        } else {
            // Test runtimes have no process.
            PaneShellState::Ready
        };
        Some(PaneShellStateInfo {
            pane_id: self.public_pane_id(ws_idx, pane)?,
            state,
            output_seq: runtime.content_seq(),
        })
    }

    pub(crate) fn handle_pane_shell_state(
        &mut self,
        id: String,
        params: PaneShellStateParams,
    ) -> String {
        match self.pane_shell_state(&params.pane_id) {
            Some(shell) => encode_success(id, ResponseResult::PaneShellState { shell }),
            None => encode_error(
                id,
                "pane_not_found",
                format!("pane {} not found", params.pane_id),
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_line_editor_shell_is_starting_until_its_editor_reads() {
        assert_eq!(
            idle_shell_state("zsh", Some(true)),
            PaneShellState::Starting
        );
        assert_eq!(idle_shell_state("-zsh", Some(false)), PaneShellState::Ready);
        assert_eq!(
            idle_shell_state("/bin/bash", Some(true)),
            PaneShellState::Starting
        );
        assert_eq!(idle_shell_state("fish", None), PaneShellState::Ready);
        // A shell without a line editor reads lines at its prompt too.
        assert_eq!(idle_shell_state("dash", Some(true)), PaneShellState::Ready);
        assert_eq!(idle_shell_state("sh", Some(true)), PaneShellState::Ready);
    }
}
