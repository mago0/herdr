//! Agent launches that any tool can rely on: the shell state a launch waits for, and resuming a
//! recorded agent session in a pane.

use crate::agent_resume::{AgentResumePlan, PersistedAgentSession};
use crate::api::schema::{
    AgentResumeParams, AgentSessionInfo, AgentSessionSpec, AgentStartParams, ErrorBody,
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

/// Why a session cannot be resumed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ResumeRefusal {
    UnknownKind(String),
    NoResumeCommand(String),
    InvalidSession(String),
}

impl ResumeRefusal {
    fn body(&self) -> ErrorBody {
        match self {
            ResumeRefusal::UnknownKind(agent) => ErrorBody {
                code: "unsupported_agent_kind".into(),
                message: format!("unsupported interactive agent kind {agent}"),
            },
            ResumeRefusal::NoResumeCommand(agent) => ErrorBody {
                code: "agent_resume_unsupported".into(),
                message: format!("herdr has no resume command for {agent} sessions"),
            },
            ResumeRefusal::InvalidSession(message) => ErrorBody {
                code: "invalid_agent_session".into(),
                message: message.clone(),
            },
        }
    }
}

/// A checked resume: the agent start that runs it and the session to record for the pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResumeLaunch {
    pub(crate) kind: String,
    pub(crate) args: Vec<String>,
    pub(crate) session: PersistedAgentSession,
}

fn encode_error_body(id: String, body: ErrorBody) -> String {
    encode_error(id, &body.code, body.message)
}

/// Herdr's own integration source for an agent label.
fn default_source(agent: &str) -> String {
    match agent {
        "agy" => "herdr:antigravity_cli".into(),
        _ => format!("herdr:{agent}"),
    }
}

/// Builds the agent arguments that resume `spec`, with `extra` native arguments. Extra arguments
/// go after the executable, or after a subcommand such as `codex resume`.
pub(crate) fn plan_resume(
    spec: &AgentSessionSpec,
    extra: &[String],
) -> Result<ResumeLaunch, ResumeRefusal> {
    let Some(kind) = crate::detect::parse_agent_label(&spec.agent) else {
        return Err(ResumeRefusal::UnknownKind(spec.agent.clone()));
    };
    let agent = crate::detect::agent_label(kind);
    let source = spec.source.clone().unwrap_or_else(|| default_source(agent));
    if !crate::agent_resume::is_official_agent_source(&source, agent) {
        return Err(if spec.source.is_some() {
            ResumeRefusal::InvalidSession(format!(
                "source {source} is not herdr's integration for {agent}"
            ))
        } else {
            ResumeRefusal::NoResumeCommand(agent.to_string())
        });
    }
    let Some(session) =
        crate::agent_resume::session_ref_from_snapshot(&source, agent, spec.kind, &spec.value)
    else {
        let kind = match spec.kind {
            crate::agent_resume::AgentSessionRefKind::Id => "id",
            crate::agent_resume::AgentSessionRefKind::Path => "path",
        };
        return Err(ResumeRefusal::InvalidSession(format!(
            "{:?} is not a valid {agent} session {kind}",
            spec.value
        )));
    };
    let Some(AgentResumePlan { argv, .. }) =
        crate::agent_resume::plan(&source, agent, &session.session_ref)
    else {
        return Err(ResumeRefusal::NoResumeCommand(agent.to_string()));
    };
    let Some((executable, rest)) = argv.split_first() else {
        return Err(ResumeRefusal::NoResumeCommand(agent.to_string()));
    };
    if executable != crate::detect::interactive_agent_executable(kind) {
        return Err(ResumeRefusal::NoResumeCommand(agent.to_string()));
    }
    let subcommand = rest
        .first()
        .filter(|word| !word.starts_with('-'))
        .map_or(0, |_| 1);
    let mut args = rest[..subcommand].to_vec();
    args.extend(extra.iter().cloned());
    args.extend(rest[subcommand..].iter().cloned());
    Ok(ResumeLaunch {
        kind: agent.to_string(),
        args,
        session,
    })
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

    /// The session that a pane or agent target records, in the shape `agent.get` reports.
    fn recorded_session(&self, target: &str) -> Result<AgentSessionSpec, ErrorBody> {
        let resolved = self
            .resolve_terminal_target(target)
            .map_err(|err| self.agent_target_error_body(err))?;
        let session = self
            .pane_info(resolved.ws_idx, resolved.pane_id)
            .and_then(|pane| pane.agent_session)
            .ok_or_else(|| ErrorBody {
                code: "agent_session_not_found".into(),
                message: format!("{target} records no agent session"),
            })?;
        Ok(AgentSessionSpec {
            agent: session.agent,
            kind: session.kind,
            value: session.value,
            source: Some(session.source),
        })
    }

    pub(crate) fn handle_agent_resume(&mut self, id: String, params: AgentResumeParams) -> String {
        let spec = match (params.session, params.from.as_deref()) {
            (Some(spec), None) => spec,
            (None, Some(from)) => match self.recorded_session(from) {
                Ok(spec) => spec,
                Err(body) => return encode_error_body(id, body),
            },
            _ => {
                return encode_error(
                    id,
                    "invalid_request",
                    "agent.resume takes either session or from",
                )
            }
        };
        let Some(pane_id) = params.pane_id else {
            return encode_error(
                id,
                "invalid_request",
                "agent.resume needs pane_id, or cwd to open a tab",
            );
        };
        let launch = match plan_resume(&spec, &params.args) {
            Ok(launch) => launch,
            Err(refusal) => return encode_error_body(id, refusal.body()),
        };
        let (agent, argv) = match self.start_agent(AgentStartParams {
            name: params.name,
            kind: launch.kind,
            pane_id,
            args: launch.args,
            timeout_ms: params.timeout_ms,
        }) {
            Ok(started) => started,
            Err(err) => return encode_error_body(id, self.agent_start_error_body(err)),
        };
        // Restore after a restart resumes this session before the agent reports it.
        if let Some(terminal) = self.state.terminals.get_mut(agent.terminal_id.as_str()) {
            terminal.set_managed_agent_launch_session(launch.session.clone());
            self.state.mark_session_dirty();
            self.schedule_session_save();
        }
        let agent = self
            .agent_info_for_target(&agent.terminal_id)
            .unwrap_or(agent);
        let session = AgentSessionInfo {
            source: launch.session.source,
            agent: launch.session.agent,
            kind: launch.session.session_ref.kind,
            value: launch.session.session_ref.value,
        };
        encode_success(
            id,
            ResponseResult::AgentResumed {
                agent,
                argv,
                session,
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_resume::AgentSessionRefKind;

    fn spec(agent: &str, kind: AgentSessionRefKind, value: &str) -> AgentSessionSpec {
        AgentSessionSpec {
            agent: agent.into(),
            kind,
            value: value.into(),
            source: None,
        }
    }

    fn args(launch: &ResumeLaunch) -> Vec<&str> {
        launch.args.iter().map(String::as_str).collect()
    }

    #[test]
    fn extra_args_follow_the_executable_or_its_subcommand() {
        let extra = vec!["--model".to_string(), "opus".to_string()];
        let claude = plan_resume(&spec("claude", AgentSessionRefKind::Id, "abc"), &extra).unwrap();
        assert_eq!(claude.kind, "claude");
        assert_eq!(args(&claude), ["--model", "opus", "--resume", "abc"]);

        let codex = plan_resume(&spec("codex", AgentSessionRefKind::Id, "s1"), &extra).unwrap();
        assert_eq!(args(&codex), ["resume", "--model", "opus", "s1"]);

        let pi = plan_resume(&spec("pi", AgentSessionRefKind::Path, "/tmp/s.jsonl"), &[]).unwrap();
        assert_eq!(args(&pi), ["--session", "/tmp/s.jsonl"]);

        let omp = plan_resume(&spec("omp", AgentSessionRefKind::Id, "01ab"), &extra).unwrap();
        assert_eq!(args(&omp), ["--model", "opus", "--resume=01ab"]);
    }

    #[test]
    fn the_session_records_herdr_source_unless_given() {
        let launch = plan_resume(&spec("claude", AgentSessionRefKind::Id, "abc"), &[]).unwrap();
        assert_eq!(launch.session.source, "herdr:claude");
        assert_eq!(launch.session.agent, "claude");
        assert_eq!(launch.session.session_ref.value, "abc");

        let agy = plan_resume(&spec("antigravity", AgentSessionRefKind::Id, "c1"), &[]).unwrap();
        assert_eq!(agy.kind, "agy");
        assert_eq!(agy.session.source, "herdr:antigravity_cli");
        assert_eq!(args(&agy), ["--conversation", "c1"]);
    }

    #[test]
    fn every_resume_plan_runs_the_executable_agent_start_types() {
        for agent in [
            "claude",
            "codex",
            "copilot",
            "devin",
            "droid",
            "kimi",
            "mastracode",
            "pi",
            "omp",
            "hermes",
            "opencode",
            "qodercli",
            "qwen",
            "kilo",
            "cursor",
            "agy",
            "grok",
            "letta",
        ] {
            assert!(
                plan_resume(&spec(agent, AgentSessionRefKind::Id, "s1"), &[]).is_ok(),
                "{agent}"
            );
        }
    }

    #[test]
    fn refuses_kinds_without_a_resume_command_and_invalid_sessions() {
        assert_eq!(
            plan_resume(&spec("gemini", AgentSessionRefKind::Id, "s1"), &[]),
            Err(ResumeRefusal::NoResumeCommand("gemini".into()))
        );
        assert_eq!(
            plan_resume(&spec("nope", AgentSessionRefKind::Id, "s1"), &[]),
            Err(ResumeRefusal::UnknownKind("nope".into()))
        );
        assert!(matches!(
            plan_resume(&spec("claude", AgentSessionRefKind::Path, "/tmp/x"), &[]),
            Err(ResumeRefusal::InvalidSession(_))
        ));
        assert!(matches!(
            plan_resume(&spec("pi", AgentSessionRefKind::Path, "relative"), &[]),
            Err(ResumeRefusal::InvalidSession(_))
        ));
        assert!(matches!(
            plan_resume(&spec("claude", AgentSessionRefKind::Id, ""), &[]),
            Err(ResumeRefusal::InvalidSession(_))
        ));
        let mut foreign = spec("claude", AgentSessionRefKind::Id, "abc");
        foreign.source = Some("custom:claude".into());
        assert!(matches!(
            plan_resume(&foreign, &[]),
            Err(ResumeRefusal::InvalidSession(_))
        ));
        assert_eq!(
            plan_resume(&spec("letta", AgentSessionRefKind::Id, "default:"), &[]),
            Err(ResumeRefusal::NoResumeCommand("letta".into()))
        );
    }

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
