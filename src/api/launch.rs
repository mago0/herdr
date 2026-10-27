//! Holds an agent launch until the target pane's shell can take a whole command line. Text typed
//! while the shell still runs its startup files can be read or dropped by a startup step.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::api::schema::{
    Method, PaneShellState, PaneShellStateInfo, PaneShellStateParams, Request, TabCreateParams,
};
use crate::api::server::{
    dispatch_to_app_with_timeout, should_stop_connection, APP_RESPONSE_TIMEOUT,
};
use crate::api::ApiRequestSender;
use crate::app::orchestration::launch::{check_launch_prompt, plan_resume};
use crate::ipc::LocalStream;

const POLL_INTERVAL: Duration = Duration::from_millis(25);
/// How long the shell must stay ready with no output. A startup step that reads the terminal
/// turns off line-at-a-time input for a moment too, but not for this long without output.
const SETTLE: Duration = Duration::from_millis(200);
/// The longest a launch waits for a starting shell. After it the launch goes ahead as before.
const MAX_WAIT: Duration = Duration::from_secs(10);
const DEFAULT_TIMEOUT_MS: u64 = 30_000;

/// What the connection does after the launch wait.
pub(super) enum Prepared {
    Request(Box<Request>),
    /// The launch failed before it reached the app, for example a tab that did not open.
    Reply(String),
    Disconnected,
}

/// Waits, for an agent launch request, until its pane's shell is ready. Other requests pass.
/// A resume with `cwd` and no pane first opens a tab for it.
pub(super) fn prepare_agent_launch(
    mut request: Request,
    stream: &mut LocalStream,
    api_tx: &ApiRequestSender,
    running: &Arc<AtomicBool>,
) -> std::io::Result<Prepared> {
    let (pane_id, timeout_ms) = match &mut request.method {
        Method::AgentStart(params) => (params.pane_id.clone(), params.timeout_ms),
        Method::AgentResume(params) => {
            let pane_id = match (params.pane_id.clone(), params.cwd.take()) {
                (Some(_), Some(_)) => {
                    return Ok(Prepared::Reply(error_reply(
                        &request.id,
                        "invalid_request",
                        "agent.resume takes pane_id or cwd, not both",
                    )))
                }
                (Some(pane_id), None) => pane_id,
                (None, Some(cwd)) => {
                    // A resume that cannot start opens no tab.
                    let refused = params
                        .session
                        .as_ref()
                        .and_then(|spec| plan_resume(spec, &params.args).err())
                        .map(|refusal| refusal.body())
                        .or_else(|| check_launch_prompt(params.prompt.as_deref()).err());
                    if let Some(body) = refused {
                        return Ok(Prepared::Reply(error_reply(
                            &request.id,
                            &body.code,
                            &body.message,
                        )));
                    }
                    match open_tab(&request.id, cwd, params.workspace_id.clone(), api_tx) {
                        Ok(pane_id) => {
                            params.pane_id = Some(pane_id.clone());
                            pane_id
                        }
                        Err(reply) => return Ok(Prepared::Reply(reply)),
                    }
                }
                (None, None) => return Ok(Prepared::Request(Box::new(request))),
            };
            (pane_id, params.timeout_ms)
        }
        _ => return Ok(Prepared::Request(Box::new(request))),
    };
    let limit = Duration::from_millis(timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS)).min(MAX_WAIT);
    let deadline = Instant::now() + limit;
    let mut settle = Settle::default();
    loop {
        if should_stop_connection(stream, running)? {
            return Ok(Prepared::Disconnected);
        }
        let Some(shell) = shell_state(&request.id, &pane_id, api_tx) else {
            // The launch itself reports a pane that is gone.
            return Ok(Prepared::Request(Box::new(request)));
        };
        let now = Instant::now();
        if settle.observe(&shell, now) || now >= deadline {
            return Ok(Prepared::Request(Box::new(request)));
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

fn error_reply(request_id: &str, code: &str, message: &str) -> String {
    serde_json::json!({
        "id": request_id,
        "error": { "code": code, "message": message },
    })
    .to_string()
}

/// Opens an unfocused tab in `cwd` and returns its root pane.
fn open_tab(
    request_id: &str,
    cwd: String,
    workspace_id: Option<String>,
    api_tx: &ApiRequestSender,
) -> Result<String, String> {
    let response = dispatch_to_app_with_timeout(
        Request {
            id: request_id.to_string(),
            method: Method::TabCreate(TabCreateParams {
                workspace_id,
                cwd: Some(cwd),
                focus: false,
                label: None,
                env: Default::default(),
            }),
        },
        api_tx,
        Some(APP_RESPONSE_TIMEOUT),
    );
    let value: serde_json::Value = serde_json::from_str(&response).map_err(|_| response.clone())?;
    value["result"]["root_pane"]["pane_id"]
        .as_str()
        .map(str::to_owned)
        .ok_or(response)
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
