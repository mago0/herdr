//! `herdr agent resume`: start an agent again on a session it recorded.

use crate::agent_resume::AgentSessionRefKind;
use crate::api::schema::{AgentResumeParams, AgentSessionSpec, Method};

const RESUME_USAGE: &str = "usage: herdr agent resume <name> (--agent-session JSON | --kind KIND --session-id ID | --kind KIND --session-path PATH | --from TARGET) (--pane ID | --cwd DIR [--workspace ID]) [--timeout MS] [--no-supervisor] [-- <agent-args...>]";

#[derive(Debug, Default, PartialEq, Eq)]
struct ResumeArgs {
    name: String,
    session: Option<AgentSessionSpec>,
    from: Option<String>,
    pane_id: Option<String>,
    cwd: Option<String>,
    workspace_id: Option<String>,
    timeout_ms: Option<u64>,
    link_supervisor: bool,
    agent_args: Vec<String>,
}

fn parse_resume_args(args: &[String]) -> Result<ResumeArgs, String> {
    let Some(name) = args.first().filter(|name| !name.starts_with("--")) else {
        return Err(RESUME_USAGE.into());
    };
    let separator = args
        .iter()
        .position(|arg| arg == "--")
        .unwrap_or(args.len());
    let mut parsed = ResumeArgs {
        name: name.clone(),
        link_supervisor: true,
        agent_args: args.get(separator + 1..).unwrap_or_default().to_vec(),
        ..ResumeArgs::default()
    };
    let mut session_json = None;
    let mut kind = None;
    let mut session_ref = None;
    let mut index = 1;
    while index < separator {
        let flag = args[index].as_str();
        if flag == "--no-supervisor" {
            parsed.link_supervisor = false;
            index += 1;
            continue;
        }
        let Some(value) = args.get(index + 1).filter(|_| index + 1 < separator) else {
            return Err(format!("missing value for {flag}"));
        };
        match flag {
            "--agent-session" => session_json = Some(value.clone()),
            "--kind" => kind = Some(value.clone()),
            "--session-id" => session_ref = Some((AgentSessionRefKind::Id, value.clone())),
            "--session-path" => session_ref = Some((AgentSessionRefKind::Path, value.clone())),
            "--from" => parsed.from = Some(value.clone()),
            "--pane" => parsed.pane_id = Some(super::normalize_pane_id(value)),
            "--cwd" => parsed.cwd = Some(value.clone()),
            "--workspace" => parsed.workspace_id = Some(value.clone()),
            "--timeout" => {
                parsed.timeout_ms = Some(
                    value
                        .parse()
                        .map_err(|_| format!("invalid --timeout value: {value}"))?,
                )
            }
            other => return Err(format!("unknown option: {other}")),
        }
        index += 2;
    }
    parsed.session = match (session_json, kind, session_ref) {
        (Some(json), None, None) => Some(parse_session_json(&json)?),
        (None, Some(agent), Some((kind, value))) => Some(AgentSessionSpec {
            agent,
            kind,
            value,
            source: None,
        }),
        (None, None, None) => None,
        (None, Some(_), None) => return Err("--kind needs --session-id or --session-path".into()),
        (None, None, Some(_)) => return Err("--session-id and --session-path need --kind".into()),
        _ => return Err("--agent-session takes the place of --kind and --session-id".into()),
    };
    if parsed.session.is_some() == parsed.from.is_some() {
        return Err("give the session to resume, or --from a pane that records one".into());
    }
    if parsed.pane_id.is_some() == parsed.cwd.is_some() {
        return Err("give --pane, or --cwd to open a new tab".into());
    }
    if parsed.workspace_id.is_some() && parsed.cwd.is_none() {
        return Err("--workspace needs --cwd".into());
    }
    if parsed.cwd.is_some() && parsed.workspace_id.is_none() {
        parsed.workspace_id = caller_workspace_id();
    }
    Ok(parsed)
}

/// The workspace of the calling pane, where `--cwd` opens its tab by default.
fn caller_workspace_id() -> Option<String> {
    if super::target::is_remote() {
        return None;
    }
    std::env::var("HERDR_WORKSPACE_ID")
        .ok()
        .filter(|value| !value.trim().is_empty())
}

/// Reads a session as `agent get` prints it in `agent_session`, alone or inside an agent record.
fn parse_session_json(json: &str) -> Result<AgentSessionSpec, String> {
    let value: serde_json::Value =
        serde_json::from_str(json).map_err(|err| format!("invalid --agent-session JSON: {err}"))?;
    let session = value
        .pointer("/result/agent/agent_session")
        .or_else(|| value.get("agent_session"))
        .unwrap_or(&value);
    serde_json::from_value(session.clone())
        .map_err(|err| format!("invalid --agent-session JSON: {err}"))
}

pub(super) fn agent_resume(args: &[String]) -> std::io::Result<i32> {
    if matches!(
        args.first().map(String::as_str),
        Some("help" | "--help" | "-h")
    ) {
        eprintln!("{RESUME_USAGE}");
        return Ok(0);
    }
    let parsed = match parse_resume_args(args) {
        Ok(parsed) => parsed,
        Err(message) => {
            eprintln!("{message}");
            return Ok(2);
        }
    };
    super::agent::launch_agent(super::agent::AgentLaunch {
        name: parsed.name.clone(),
        pane_id: parsed.pane_id.clone(),
        expected_kind: None,
        timeout_ms: parsed.timeout_ms,
        link_supervisor: parsed.link_supervisor,
        method: Method::AgentResume(AgentResumeParams {
            name: parsed.name,
            session: parsed.session,
            from: parsed.from,
            pane_id: parsed.pane_id,
            cwd: parsed.cwd,
            workspace_id: parsed.workspace_id,
            args: parsed.agent_args,
            timeout_ms: parsed.timeout_ms,
        }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(words: &[&str]) -> Vec<String> {
        words.iter().map(|word| word.to_string()).collect()
    }

    #[test]
    fn parses_an_explicit_session_and_trailing_args() {
        let parsed = parse_resume_args(&args(&[
            "worker",
            "--kind",
            "claude",
            "--session-id",
            "abc",
            "--pane",
            "w1:p2",
            "--no-supervisor",
            "--",
            "--model",
            "opus",
        ]))
        .unwrap();
        assert_eq!(parsed.name, "worker");
        assert_eq!(
            parsed.session,
            Some(AgentSessionSpec {
                agent: "claude".into(),
                kind: AgentSessionRefKind::Id,
                value: "abc".into(),
                source: None,
            })
        );
        assert_eq!(parsed.pane_id.as_deref(), Some("w1:p2"));
        assert!(!parsed.link_supervisor);
        assert_eq!(parsed.agent_args, ["--model", "opus"]);
    }

    #[test]
    fn reads_the_session_json_that_agent_get_prints() {
        let session = r#"{"source":"herdr:pi","agent":"pi","kind":"path","value":"/tmp/s.jsonl"}"#;
        let record = format!(r#"{{"id":"x","result":{{"agent":{{"agent_session":{session}}}}}}}"#);
        for json in [session.to_string(), record] {
            let parsed =
                parse_resume_args(&args(&["w", "--agent-session", &json, "--cwd", "/tmp"]))
                    .unwrap();
            let spec = parsed.session.unwrap();
            assert_eq!(spec.agent, "pi");
            assert_eq!(spec.kind, AgentSessionRefKind::Path);
            assert_eq!(spec.source.as_deref(), Some("herdr:pi"));
            assert_eq!(parsed.cwd.as_deref(), Some("/tmp"));
        }
    }

    #[test]
    fn rejects_missing_or_conflicting_sources_and_targets() {
        for bad in [
            &["w", "--pane", "w1:p1"][..],
            &["w", "--from", "w1:p1"],
            &[
                "w",
                "--from",
                "a",
                "--kind",
                "pi",
                "--session-id",
                "x",
                "--pane",
                "w1:p1",
            ],
            &["w", "--kind", "pi", "--pane", "w1:p1"],
            &["w", "--session-id", "x", "--pane", "w1:p1"],
            &["w", "--from", "a", "--pane", "w1:p1", "--cwd", "/tmp"],
            &["w", "--from", "a", "--pane", "w1:p1", "--workspace", "w1"],
            &["w", "--from", "a", "--pane", "w1:p1", "--timeout", "soon"],
            &["w", "--from", "a", "--pane"],
            &["--from", "a", "--pane", "w1:p1"],
        ] {
            assert!(parse_resume_args(&args(bad)).is_err(), "{bad:?}");
        }
    }
}
