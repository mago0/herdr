//! Commands for deliveries, supervision links, jobs and event history.

use crate::api::schema::{
    AgentCancelDeliveryParams, AgentDeliverParams, AgentDeliveriesParams, EmptyParams,
    EventsReadParams, JobAddParams, JobTarget, Method, PaneSuperviseParams, Request,
    SuperviseReport,
};

const DELIVER_USAGE: &str =
    "usage: herdr agent deliver <target> <text> [--source NAME] [--ttl DURATION]";
const SUPERVISE_USAGE: &str = "usage: herdr pane supervise <pane> --by <agent|pane> [--report blocked,exited,stalled]\n       herdr pane supervise <pane> --clear";
const JOB_ADD_USAGE: &str = "usage: herdr job add <name> [--every DURATION] [--on EVENT]... [--path FILE] [--to <agent|pane>] [--cwd DIR] [--env NAME=VALUE]... [--timeout DURATION] [--until DURATION] [--no-dedupe] [--owner PANE|--no-owner] -- <command...>";
const EVENTS_READ_USAGE: &str =
    "usage: herdr events read [--after SEQ] [--kind EVENT]... [--limit N]";

/// A time span such as `500ms`, `60s`, `5m`, `2h` or `1d`. A bare number is seconds.
pub(super) fn parse_duration_ms(value: &str) -> Result<u64, String> {
    let (digits, unit) = match value.find(|c: char| !c.is_ascii_digit()) {
        Some(split) => value.split_at(split),
        None => (value, "s"),
    };
    let amount: u64 = digits
        .parse()
        .map_err(|_| format!("invalid duration: {value}"))?;
    let factor = match unit {
        "ms" => 1,
        "s" => 1_000,
        "m" => 60_000,
        "h" => 3_600_000,
        "d" => 86_400_000,
        _ => return Err(format!("invalid duration: {value} (use ms, s, m, h or d)")),
    };
    amount
        .checked_mul(factor)
        .ok_or_else(|| format!("duration is too large: {value}"))
}

fn value_of<'a>(args: &'a [String], index: usize, flag: &str) -> Result<&'a String, String> {
    args.get(index + 1)
        .ok_or_else(|| format!("missing value for {flag}"))
}

pub(super) fn parse_deliver(args: &[String]) -> Result<AgentDeliverParams, String> {
    let (mut positional, mut source, mut ttl_ms) = (Vec::new(), None, None);
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--source" => {
                source = Some(value_of(args, index, "--source")?.clone());
                index += 2;
            }
            "--ttl" => {
                ttl_ms = Some(parse_duration_ms(value_of(args, index, "--ttl")?)?);
                index += 2;
            }
            other if other.starts_with("--") => return Err(format!("unknown option: {other}")),
            _ => {
                positional.push(args[index].clone());
                index += 1;
            }
        }
    }
    let [target, text] = positional.as_slice() else {
        return Err(DELIVER_USAGE.into());
    };
    Ok(AgentDeliverParams {
        target: target.clone(),
        text: text.clone(),
        source,
        ttl_ms,
    })
}

pub(super) fn parse_report(value: &str) -> Result<Vec<SuperviseReport>, String> {
    value
        .split(',')
        .filter(|part| !part.is_empty())
        .map(|part| match part {
            "blocked" => Ok(SuperviseReport::Blocked),
            "exited" => Ok(SuperviseReport::Exited),
            "stalled" => Ok(SuperviseReport::Stalled),
            other => Err(format!(
                "invalid --report value: {other} (expected blocked, exited or stalled)"
            )),
        })
        .collect()
}

pub(super) fn parse_supervise(args: &[String]) -> Result<PaneSuperviseParams, String> {
    let (mut pane, mut by, mut report, mut clear) = (None, None, Vec::new(), false);
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--by" => {
                by = Some(value_of(args, index, "--by")?.clone());
                index += 2;
            }
            "--report" => {
                report = parse_report(value_of(args, index, "--report")?)?;
                index += 2;
            }
            "--clear" => {
                clear = true;
                index += 1;
            }
            other if other.starts_with("--") => return Err(format!("unknown option: {other}")),
            _ if pane.is_none() => {
                pane = Some(args[index].clone());
                index += 1;
            }
            _ => return Err(SUPERVISE_USAGE.into()),
        }
    }
    let Some(pane_id) = pane else {
        return Err(SUPERVISE_USAGE.into());
    };
    match (by, clear) {
        (Some(_), true) => Err("--by and --clear cannot be used together".into()),
        (None, false) => Err(SUPERVISE_USAGE.into()),
        (supervisor, _) => Ok(PaneSuperviseParams {
            pane_id,
            supervisor,
            report,
        }),
    }
}

/// What the command line cannot know by itself when it builds a job.
pub(super) struct JobDefaults {
    pub(super) caller_pane: Option<String>,
    pub(super) cwd: Option<String>,
    pub(super) now_ms: u64,
}

pub(super) fn parse_job_add(
    args: &[String],
    defaults: JobDefaults,
) -> Result<JobAddParams, String> {
    let separator = args
        .iter()
        .position(|arg| arg == "--")
        .ok_or(JOB_ADD_USAGE)?;
    let command = args[separator + 1..].to_vec();
    let options = &args[..separator];
    let Some(name) = options.first().filter(|name| !name.starts_with("--")) else {
        return Err(JOB_ADD_USAGE.into());
    };
    let mut params = JobAddParams {
        name: name.clone(),
        command,
        cwd: defaults.cwd,
        env: Default::default(),
        every_ms: None,
        on_events: Vec::new(),
        on_path: None,
        target: None,
        owner_pane_id: defaults.caller_pane,
        timeout_ms: None,
        until_ms: None,
        dedupe: None,
    };
    let mut index = 1;
    while index < options.len() {
        let flag = options[index].as_str();
        if flag == "--no-dedupe" {
            params.dedupe = Some(false);
            index += 1;
            continue;
        }
        if flag == "--no-owner" {
            params.owner_pane_id = None;
            index += 1;
            continue;
        }
        let value = value_of(options, index, flag)?;
        match flag {
            "--every" => params.every_ms = Some(parse_duration_ms(value)?),
            "--on" => params.on_events.push(value.clone()),
            "--path" => params.on_path = Some(value.clone()),
            "--to" => params.target = Some(value.clone()),
            "--cwd" => params.cwd = Some(value.clone()),
            "--owner" => params.owner_pane_id = Some(value.clone()),
            "--timeout" => params.timeout_ms = Some(parse_duration_ms(value)?),
            "--until" => {
                params.until_ms = Some(defaults.now_ms.saturating_add(parse_duration_ms(value)?))
            }
            "--env" => {
                let (key, env_value) = super::parse_env_assignment(value)?;
                params.env.insert(key, env_value);
            }
            other => return Err(format!("unknown option: {other}")),
        }
        index += 2;
    }
    Ok(params)
}

pub(super) fn parse_events_read(args: &[String]) -> Result<EventsReadParams, String> {
    let mut params = EventsReadParams::default();
    let mut index = 0;
    while index < args.len() {
        let flag = args[index].as_str();
        let value = value_of(args, index, flag)?;
        match flag {
            "--after" => {
                params.after = Some(
                    value
                        .parse()
                        .map_err(|_| format!("invalid --after: {value}"))?,
                )
            }
            "--kind" => params.kinds.push(value.clone()),
            "--limit" => {
                params.limit = Some(
                    value
                        .parse()
                        .map_err(|_| format!("invalid --limit: {value}"))?,
                )
            }
            other => return Err(format!("unknown option: {other}")),
        }
        index += 2;
    }
    Ok(params)
}

fn send(id: &str, method: Method) -> std::io::Result<i32> {
    super::print_response(&super::send_request(&Request {
        id: id.into(),
        method,
    })?)
}

fn usage_error(message: String) -> std::io::Result<i32> {
    eprintln!("{message}");
    Ok(2)
}

pub(super) fn agent_deliver(args: &[String]) -> std::io::Result<i32> {
    match parse_deliver(args) {
        Ok(params) => send("cli:agent:deliver", Method::AgentDeliver(params)),
        Err(message) => usage_error(message),
    }
}

pub(super) fn agent_deliveries(args: &[String]) -> std::io::Result<i32> {
    let target = match args {
        [] => None,
        [target] if !target.starts_with("--") => Some(target.clone()),
        _ => return usage_error("usage: herdr agent deliveries [<target>]".into()),
    };
    send(
        "cli:agent:deliveries",
        Method::AgentDeliveries(AgentDeliveriesParams { target }),
    )
}

pub(super) fn agent_cancel_delivery(args: &[String]) -> std::io::Result<i32> {
    let [delivery_id] = args else {
        return usage_error("usage: herdr agent cancel-delivery <delivery-id>".into());
    };
    send(
        "cli:agent:cancel-delivery",
        Method::AgentCancelDelivery(AgentCancelDeliveryParams {
            delivery_id: delivery_id.clone(),
        }),
    )
}

pub(super) fn pane_supervise(args: &[String]) -> std::io::Result<i32> {
    match parse_supervise(args) {
        Ok(params) => send("cli:pane:supervise", Method::PaneSupervise(params)),
        Err(message) => usage_error(message),
    }
}

/// Makes the calling pane the supervisor of a pane it started an agent in.
pub(super) fn supervise_from_caller(pane_id: &str) {
    let Some(caller) = super::target::caller_pane_id() else {
        return;
    };
    if caller == pane_id {
        return;
    }
    // The link is a courtesy: an older server without the method must not fail the start.
    let _ = super::send_request_unchecked(&Request {
        id: "cli:agent:start:supervise".into(),
        method: Method::PaneSupervise(PaneSuperviseParams {
            pane_id: pane_id.to_string(),
            supervisor: Some(caller),
            report: Vec::new(),
        }),
    });
}

pub(super) fn run_job_command(args: &[String]) -> std::io::Result<i32> {
    let name_only = |args: &[String], verb: &str| match args {
        [name] if !name.starts_with("--") => Ok(JobTarget { name: name.clone() }),
        _ => Err(format!("usage: herdr job {verb} <name>")),
    };
    match args.first().map(String::as_str) {
        Some("add") => {
            let defaults = JobDefaults {
                caller_pane: super::target::caller_pane_id(),
                cwd: std::env::current_dir()
                    .ok()
                    .map(|cwd| cwd.display().to_string()),
                now_ms: super::unix_ms_now(),
            };
            match parse_job_add(&args[1..], defaults) {
                Ok(params) => send("cli:job:add", Method::JobAdd(params)),
                Err(message) => usage_error(message),
            }
        }
        Some("list") if args.len() == 1 => {
            send("cli:job:list", Method::JobList(EmptyParams::default()))
        }
        Some("remove") => match name_only(&args[1..], "remove") {
            Ok(target) => send("cli:job:remove", Method::JobRemove(target)),
            Err(message) => usage_error(message),
        },
        Some("run") => match name_only(&args[1..], "run") {
            Ok(target) => send("cli:job:run", Method::JobRun(target)),
            Err(message) => usage_error(message),
        },
        Some("help" | "--help" | "-h") => {
            print_job_help();
            Ok(0)
        }
        _ => {
            print_job_help();
            Ok(2)
        }
    }
}

fn print_job_help() {
    eprintln!("herdr job commands:");
    eprintln!("  herdr job add <name> [--every DURATION] [--on EVENT]... [--path FILE] [--to <agent|pane>]");
    eprintln!(
        "                [--cwd DIR] [--env NAME=VALUE]... [--timeout DURATION] [--until DURATION]"
    );
    eprintln!("                [--no-dedupe] [--owner PANE|--no-owner] -- <command...>");
    eprintln!("  herdr job list");
    eprintln!("  herdr job run <name>");
    eprintln!("  herdr job remove <name>");
    eprintln!("  a run that exits 0 and prints text has news; --to delivers that text to an agent");
    eprintln!("  durations: 500ms, 60s, 5m, 2h, 1d; a bare number is seconds");
}

pub(super) fn run_events_command(args: &[String]) -> std::io::Result<i32> {
    match args.first().map(String::as_str) {
        Some("read") => match parse_events_read(&args[1..]) {
            Ok(params) => send("cli:events:read", Method::EventsRead(params)),
            Err(message) => usage_error(message),
        },
        Some("help" | "--help" | "-h") => {
            eprintln!("{EVENTS_READ_USAGE}");
            Ok(0)
        }
        _ => {
            eprintln!("{EVENTS_READ_USAGE}");
            Ok(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| item.to_string()).collect()
    }

    fn defaults() -> JobDefaults {
        JobDefaults {
            caller_pane: Some("w1:p2".into()),
            cwd: Some("/work".into()),
            now_ms: 1_000,
        }
    }

    #[test]
    fn durations_take_a_unit_and_a_bare_number_is_seconds() {
        assert_eq!(parse_duration_ms("500ms"), Ok(500));
        assert_eq!(parse_duration_ms("60s"), Ok(60_000));
        assert_eq!(parse_duration_ms("60"), Ok(60_000));
        assert_eq!(parse_duration_ms("5m"), Ok(300_000));
        assert_eq!(parse_duration_ms("2h"), Ok(7_200_000));
        assert_eq!(parse_duration_ms("1d"), Ok(86_400_000));
        for bad in ["", "s", "5x", "-5s", "1.5h", "99999999999999999999s"] {
            assert!(parse_duration_ms(bad).is_err(), "{bad}");
        }
        assert!(parse_duration_ms("18446744073709551615d").is_err());
    }

    #[test]
    fn deliver_takes_a_target_and_one_text() {
        let params = parse_deliver(&args(&[
            "lead", "MAIL|1", "--source", "dispatch", "--ttl", "5m",
        ]))
        .unwrap();
        assert_eq!(
            params,
            AgentDeliverParams {
                target: "lead".into(),
                text: "MAIL|1".into(),
                source: Some("dispatch".into()),
                ttl_ms: Some(300_000),
            }
        );
        assert!(parse_deliver(&args(&["lead"])).is_err());
        assert!(parse_deliver(&args(&["lead", "a", "b"])).is_err());
        assert!(parse_deliver(&args(&["lead", "a", "--bogus"])).is_err());
        assert!(parse_deliver(&args(&["lead", "a", "--ttl"])).is_err());
    }

    #[test]
    fn supervise_links_with_by_and_clears_with_clear() {
        let linked = parse_supervise(&args(&[
            "w2:p1",
            "--by",
            "lead",
            "--report",
            "blocked,exited",
        ]))
        .unwrap();
        assert_eq!(
            linked,
            PaneSuperviseParams {
                pane_id: "w2:p1".into(),
                supervisor: Some("lead".into()),
                report: vec![SuperviseReport::Blocked, SuperviseReport::Exited],
            }
        );
        let cleared = parse_supervise(&args(&["w2:p1", "--clear"])).unwrap();
        assert_eq!(cleared.supervisor, None);

        assert!(parse_supervise(&args(&["w2:p1"])).is_err());
        assert!(parse_supervise(&args(&["--by", "lead"])).is_err());
        assert!(parse_supervise(&args(&["w2:p1", "--by", "lead", "--clear"])).is_err());
        assert!(parse_supervise(&args(&["w2:p1", "--by", "lead", "--report", "dead"])).is_err());
    }

    #[test]
    fn job_add_reads_options_before_the_separator_and_the_command_after_it() {
        let params = parse_job_add(
            &args(&[
                "pr-watch",
                "--every",
                "60s",
                "--on",
                "pane.closed",
                "--on",
                "pane.exited",
                "--path",
                "/tmp/p.json",
                "--to",
                "lead",
                "--env",
                "PR=12",
                "--timeout",
                "30s",
                "--until",
                "1h",
                "--no-dedupe",
                "--",
                "gh",
                "pr",
                "checks",
                "--watch",
            ]),
            defaults(),
        )
        .unwrap();
        assert_eq!(params.name, "pr-watch");
        assert_eq!(params.command, args(&["gh", "pr", "checks", "--watch"]));
        assert_eq!(params.every_ms, Some(60_000));
        assert_eq!(params.on_events, args(&["pane.closed", "pane.exited"]));
        assert_eq!(params.on_path.as_deref(), Some("/tmp/p.json"));
        assert_eq!(params.target.as_deref(), Some("lead"));
        assert_eq!(params.env.get("PR").map(String::as_str), Some("12"));
        assert_eq!(params.timeout_ms, Some(30_000));
        assert_eq!(params.until_ms, Some(3_601_000));
        assert_eq!(params.dedupe, Some(false));
        assert_eq!(params.cwd.as_deref(), Some("/work"));
        assert_eq!(params.owner_pane_id.as_deref(), Some("w1:p2"));
    }

    #[test]
    fn job_add_owner_defaults_to_the_calling_pane_and_can_be_changed_or_dropped() {
        let named = parse_job_add(&args(&["a", "--owner", "w3:p1", "--", "true"]), defaults());
        assert_eq!(named.unwrap().owner_pane_id.as_deref(), Some("w3:p1"));
        let none = parse_job_add(&args(&["a", "--no-owner", "--", "true"]), defaults());
        assert_eq!(none.unwrap().owner_pane_id, None);
        let cwd = parse_job_add(&args(&["a", "--cwd", "/other", "--", "true"]), defaults());
        assert_eq!(cwd.unwrap().cwd.as_deref(), Some("/other"));
    }

    #[test]
    fn job_add_needs_a_name_and_a_separator() {
        assert!(parse_job_add(&args(&["a", "--every", "60s", "true"]), defaults()).is_err());
        assert!(parse_job_add(&args(&["--every", "60s", "--", "true"]), defaults()).is_err());
        assert!(parse_job_add(&args(&["a", "--bogus", "x", "--", "true"]), defaults()).is_err());
        assert!(parse_job_add(&args(&["a", "--every", "--", "true"]), defaults()).is_err());
    }

    #[test]
    fn events_read_takes_a_cursor_kinds_and_a_limit() {
        let params = parse_events_read(&args(&[
            "--after",
            "41",
            "--kind",
            "pane.closed",
            "--kind",
            "pane.exited",
            "--limit",
            "10",
        ]))
        .unwrap();
        assert_eq!(params.after, Some(41));
        assert_eq!(params.kinds, args(&["pane.closed", "pane.exited"]));
        assert_eq!(params.limit, Some(10));
        assert_eq!(parse_events_read(&[]).unwrap(), EventsReadParams::default());
        assert!(parse_events_read(&args(&["--after", "x"])).is_err());
        assert!(parse_events_read(&args(&["--after"])).is_err());
    }
}
