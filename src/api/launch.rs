//! Holds an agent launch until the target pane's shell can take a whole command line. Text typed
//! while the shell still runs its startup files can be read or dropped by a startup step.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::api::schema::{
    Method, PaneShellState, PaneShellStateInfo, PaneShellStateParams, Request,
};
use crate::api::server::{
    dispatch_to_app_with_timeout, should_stop_connection, APP_RESPONSE_TIMEOUT,
};
use crate::api::ApiRequestSender;
use crate::ipc::LocalStream;

const POLL_INTERVAL: Duration = Duration::from_millis(25);
/// How long the shell must stay ready with no output. A startup step that reads the terminal
/// turns off line-at-a-time input for a moment too, but not for this long without output.
const SETTLE: Duration = Duration::from_millis(200);
/// The longest a launch waits for a starting shell. After it the launch goes ahead as before.
const MAX_WAIT: Duration = Duration::from_secs(10);
const DEFAULT_TIMEOUT_MS: u64 = 30_000;

/// Waits, for an agent launch request, until its pane's shell is ready. Other requests pass.
/// None when the client went away while it waited.
pub(super) fn prepare_agent_launch(
    request: Request,
    stream: &mut LocalStream,
    api_tx: &ApiRequestSender,
    running: &Arc<AtomicBool>,
) -> std::io::Result<Option<Request>> {
    let (pane_id, timeout_ms) = match &request.method {
        Method::AgentStart(params) => (params.pane_id.clone(), params.timeout_ms),
        _ => return Ok(Some(request)),
    };
    let limit = Duration::from_millis(timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS)).min(MAX_WAIT);
    let deadline = Instant::now() + limit;
    let mut settle = Settle::default();
    loop {
        if should_stop_connection(stream, running)? {
            return Ok(None);
        }
        let Some(shell) = shell_state(&request.id, &pane_id, api_tx) else {
            // The launch itself reports a pane that is gone.
            return Ok(Some(request));
        };
        let now = Instant::now();
        if settle.observe(&shell, now) || now >= deadline {
            return Ok(Some(request));
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

fn shell_state(
    request_id: &str,
    pane_id: &str,
    api_tx: &ApiRequestSender,
) -> Option<PaneShellStateInfo> {
    let response = dispatch_to_app_with_timeout(
        Request {
            id: format!("{request_id}:shell_state"),
            method: Method::PaneShellState(PaneShellStateParams {
                pane_id: pane_id.to_string(),
            }),
        },
        api_tx,
        Some(APP_RESPONSE_TIMEOUT),
    );
    let value: serde_json::Value = serde_json::from_str(&response).ok()?;
    serde_json::from_value(value["result"]["shell"].clone()).ok()
}

/// Tracks how long a shell has stayed ready without output.
#[derive(Default)]
struct Settle {
    ready_since: Option<(Instant, u64)>,
}

impl Settle {
    /// True when the launch can go ahead: the shell is busy with something else, which the
    /// launch reports, or it has been ready with no output for [`SETTLE`].
    fn observe(&mut self, shell: &PaneShellStateInfo, now: Instant) -> bool {
        match shell.state {
            PaneShellState::Busy => true,
            PaneShellState::Starting => {
                self.ready_since = None;
                false
            }
            PaneShellState::Ready => match self.ready_since {
                Some((since, seq)) if seq == shell.output_seq => now - since >= SETTLE,
                _ => {
                    self.ready_since = Some((now, shell.output_seq));
                    false
                }
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shell(state: PaneShellState, output_seq: u64) -> PaneShellStateInfo {
        PaneShellStateInfo {
            pane_id: "w1:p1".into(),
            state,
            output_seq,
        }
    }

    #[test]
    fn a_ready_shell_must_stay_quiet_for_the_settle_time() {
        let start = Instant::now();
        let mut settle = Settle::default();
        assert!(!settle.observe(&shell(PaneShellState::Ready, 1), start));
        assert!(!settle.observe(&shell(PaneShellState::Ready, 1), start + SETTLE / 2));
        // Output restarts the settle time.
        assert!(!settle.observe(&shell(PaneShellState::Ready, 2), start + SETTLE));
        assert!(!settle.observe(
            &shell(PaneShellState::Ready, 2),
            start + SETTLE + SETTLE / 2
        ));
        assert!(settle.observe(&shell(PaneShellState::Ready, 2), start + SETTLE * 2));
    }

    #[test]
    fn a_starting_shell_restarts_the_settle_time() {
        let start = Instant::now();
        let mut settle = Settle::default();
        assert!(!settle.observe(&shell(PaneShellState::Ready, 1), start));
        assert!(!settle.observe(&shell(PaneShellState::Starting, 1), start + SETTLE / 2));
        assert!(!settle.observe(&shell(PaneShellState::Ready, 1), start + SETTLE));
        assert!(settle.observe(&shell(PaneShellState::Ready, 1), start + SETTLE * 2));
    }

    #[test]
    fn a_busy_pane_goes_ahead_so_the_launch_reports_it() {
        let mut settle = Settle::default();
        assert!(settle.observe(&shell(PaneShellState::Busy, 1), Instant::now()));
    }
}
