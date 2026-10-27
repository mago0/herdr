use super::harness::*;

/// A fake interactive agent named `kind`. It records its arguments and each prompt line, and
/// reports itself idle through the API.
fn write_fake_agent(bin: &Path, base: &Path, kind: &str) {
    use std::os::unix::fs::PermissionsExt;

    fs::create_dir_all(bin).unwrap();
    let script = bin.join(kind);
    fs::write(
        &script,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{args}'\nexport HERDR_AGENT={kind}\n'{herdr}' pane report-agent \"$HERDR_PANE_ID\" --source custom:fake --agent {kind} --state idle >/dev/null\nwhile IFS= read -r prompt; do\n  printf '%s\\n' \"$prompt\" >> '{prompts}'\n  '{herdr}' pane report-agent \"$HERDR_PANE_ID\" --source custom:fake --agent {kind} --state working >/dev/null\n  '{herdr}' pane report-agent \"$HERDR_PANE_ID\" --source custom:fake --agent {kind} --state idle >/dev/null\ndone\n",
            args = base.join(format!("{kind}-args")).display(),
            prompts = base.join(format!("{kind}-prompts")).display(),
            herdr = env!("CARGO_BIN_EXE_herdr"),
        ),
    )
    .unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
}

fn wait_for_file(path: &Path, timeout: Duration) -> String {
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(text) = fs::read_to_string(path) {
            if !text.is_empty() {
                return text;
            }
        }
        assert!(
            Instant::now() < deadline,
            "{} was not written",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn find_on_path(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(name))
        .find(|path| path.is_file())
}

#[test]
fn agent_start_waits_until_the_shell_line_editor_reads() {
    let Some(bash) = find_on_path("bash") else {
        return;
    };
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let socket_path = runtime_dir.join("herdr.sock");
    let bin = base.join("bin");
    write_fake_agent(&bin, &base, "pi");
    // Startup work in the shell itself, then a terminal probe that reads the reply. Text typed
    // before the prompt would be read by the probe.
    let rc = base.join("bashrc");
    fs::write(
        &rc,
        "for ((i = 0; i < 200000; i++)); do :; done\nprintf '\\033[c'\nIFS= read -rs -t 2 -d c _reply\nPS1='$ '\n",
    )
    .unwrap();
    let shell = bin.join("probing-shell");
    fs::write(
        &shell,
        format!(
            "#!/bin/sh\nexec '{}' --rcfile '{}' -i\n",
            bash.display(),
            rc.display()
        ),
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&shell, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let config = format!(
        "onboarding = false\n[terminal]\ndefault_shell = {:?}\nshell_mode = \"non_login\"\n",
        shell.to_str().unwrap()
    );
    let herdr = spawn_herdr_with_config(
        &config_home,
        &runtime_dir,
        &socket_path,
        Some(&bin),
        &config,
    );
    wait_for_socket(&socket_path, Duration::from_secs(5));
    let created = run_cli_json(
        &socket_path,
        &["workspace", "create", "--cwd", base.to_str().unwrap()],
    );
    let pane_id = created["result"]["root_pane"]["pane_id"].as_str().unwrap();

    let started = run_cli_json(
        &socket_path,
        &[
            "agent",
            "start",
            "worker",
            "--kind",
            "pi",
            "--pane",
            pane_id,
            "--timeout",
            "8000",
            "--",
            "--no-context-files",
        ],
    );

    assert_eq!(started["result"]["agent"]["interactive_ready"], true);
    assert_eq!(
        fs::read_to_string(base.join("pi-args")).unwrap(),
        "--no-context-files\n"
    );

    cleanup_spawned_herdr(herdr, base);
}

fn socket_request(socket_path: &Path, request: serde_json::Value) -> serde_json::Value {
    let mut stream = UnixStream::connect(socket_path).unwrap();
    writeln!(stream, "{request}").unwrap();
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).unwrap();
    serde_json::from_str(&line).unwrap()
}

struct ResumeFixture {
    base: PathBuf,
    socket_path: PathBuf,
    herdr: SpawnedHerdr,
    pane_id: String,
}

fn resume_fixture(kinds: &[&str]) -> ResumeFixture {
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let socket_path = runtime_dir.join("herdr.sock");
    let bin = base.join("bin");
    for kind in kinds {
        write_fake_agent(&bin, &base, kind);
    }
    let herdr = spawn_herdr_with_path(&config_home, &runtime_dir, &socket_path, Some(&bin));
    wait_for_socket(&socket_path, Duration::from_secs(5));
    let created = run_cli_json(
        &socket_path,
        &["workspace", "create", "--cwd", base.to_str().unwrap()],
    );
    let pane_id = created["result"]["root_pane"]["pane_id"]
        .as_str()
        .unwrap()
        .to_string();
    ResumeFixture {
        base,
        socket_path,
        herdr,
        pane_id,
    }
}

fn cli_error(output: &std::process::Output) -> serde_json::Value {
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    serde_json::from_slice(&output.stderr).unwrap()
}

#[test]
fn agent_resume_runs_the_agents_resume_command_with_extra_args() {
    let fixture = resume_fixture(&["claude", "codex"]);
    let socket_path = &fixture.socket_path;

    let resumed = run_cli_json(
        socket_path,
        &[
            "agent",
            "resume",
            "worker",
            "--kind",
            "claude",
            "--session-id",
            "abc-123",
            "--pane",
            &fixture.pane_id,
            "--timeout",
            "8000",
            "--",
            "--model",
            "opus",
        ],
    );

    assert_eq!(resumed["result"]["type"], "agent_resumed");
    assert_eq!(
        resumed["result"]["argv"],
        serde_json::json!(["claude", "--model", "opus", "--resume", "abc-123"])
    );
    assert_eq!(
        resumed["result"]["session"],
        serde_json::json!({"source": "herdr:claude", "agent": "claude", "kind": "id", "value": "abc-123"})
    );
    assert_eq!(resumed["result"]["agent"]["name"], "worker");
    assert_eq!(resumed["result"]["agent"]["pane_id"], fixture.pane_id);
    assert_eq!(resumed["result"]["agent"]["interactive_ready"], true);
    assert_eq!(
        fs::read_to_string(fixture.base.join("claude-args")).unwrap(),
        "--model\nopus\n--resume\nabc-123\n"
    );

    // A subcommand stays first: codex resume <args> <id>.
    let created = run_cli_json(
        socket_path,
        &["tab", "create", "--cwd", fixture.base.to_str().unwrap()],
    );
    let codex_pane = created["result"]["root_pane"]["pane_id"].as_str().unwrap();
    let session = r#"{"source":"herdr:codex","agent":"codex","kind":"id","value":"s-9"}"#;
    let resumed = run_cli_json(
        socket_path,
        &[
            "agent",
            "resume",
            "coder",
            "--agent-session",
            session,
            "--pane",
            codex_pane,
            "--no-supervisor",
            "--",
            "-m",
            "o3",
        ],
    );
    assert_eq!(
        resumed["result"]["argv"],
        serde_json::json!(["codex", "resume", "-m", "o3", "s-9"])
    );

    cleanup_spawned_herdr(fixture.herdr, fixture.base);
}

#[test]
fn agent_resume_opens_a_tab_in_cwd() {
    let fixture = resume_fixture(&["pi"]);
    let socket_path = &fixture.socket_path;
    let worktree = fixture.base.join("worktree");
    fs::create_dir_all(&worktree).unwrap();
    let session_file = fixture.base.join("session.jsonl");

    let resumed = run_cli_json(
        socket_path,
        &[
            "agent",
            "resume",
            "worker",
            "--kind",
            "pi",
            "--session-path",
            session_file.to_str().unwrap(),
            "--cwd",
            worktree.to_str().unwrap(),
            "--workspace",
            fixture.pane_id.split(':').next().unwrap(),
        ],
    );

    let pane_id = resumed["result"]["agent"]["pane_id"].as_str().unwrap();
    assert_ne!(pane_id, fixture.pane_id);
    assert_eq!(
        resumed["result"]["agent"]["cwd"].as_str().unwrap(),
        worktree.to_str().unwrap()
    );
    assert_eq!(
        fs::read_to_string(fixture.base.join("pi-args")).unwrap(),
        format!("--session\n{}\n", session_file.display())
    );

    cleanup_spawned_herdr(fixture.herdr, fixture.base);
}

#[test]
fn agent_resume_refuses_sessions_it_cannot_resume_before_typing() {
    let fixture = resume_fixture(&["claude"]);
    let socket_path = &fixture.socket_path;
    let pane = fixture.pane_id.as_str();

    let unsupported = cli_error(&run_cli(
        socket_path,
        &[
            "agent",
            "resume",
            "w",
            "--kind",
            "gemini",
            "--session-id",
            "x",
            "--pane",
            pane,
        ],
    ));
    assert_eq!(unsupported["error"]["code"], "agent_resume_unsupported");

    let invalid = cli_error(&run_cli(
        socket_path,
        &[
            "agent",
            "resume",
            "w",
            "--kind",
            "claude",
            "--session-path",
            "/tmp/s",
            "--pane",
            pane,
        ],
    ));
    assert_eq!(invalid["error"]["code"], "invalid_agent_session");

    let unknown = cli_error(&run_cli(
        socket_path,
        &[
            "agent",
            "resume",
            "w",
            "--kind",
            "nope",
            "--session-id",
            "x",
            "--pane",
            pane,
        ],
    ));
    assert_eq!(unknown["error"]["code"], "unsupported_agent_kind");

    let none = cli_error(&run_cli(
        socket_path,
        &["agent", "resume", "w", "--from", pane, "--pane", pane],
    ));
    assert_eq!(none["error"]["code"], "agent_session_not_found");

    let usage = run_cli(socket_path, &["agent", "resume", "w", "--pane", pane]);
    assert_eq!(usage.status.code(), Some(2));
    assert!(!fixture.base.join("claude-args").exists());

    cleanup_spawned_herdr(fixture.herdr, fixture.base);
}

#[test]
fn agent_resume_over_the_socket_resumes_the_session_a_pane_records() {
    let fixture = resume_fixture(&["claude"]);
    let socket_path = &fixture.socket_path;
    let reported = run_cli(
        socket_path,
        &[
            "pane",
            "report-agent-session",
            &fixture.pane_id,
            "--source",
            "herdr:claude",
            "--agent",
            "claude",
            "--agent-session-id",
            "recorded-1",
        ],
    );
    assert!(reported.status.success(), "{reported:?}");
    let pane = run_cli_json(socket_path, &["pane", "get", &fixture.pane_id]);
    assert_eq!(
        pane["result"]["pane"]["agent_session"]["value"], "recorded-1",
        "{pane}"
    );
    let created = run_cli_json(
        socket_path,
        &["tab", "create", "--cwd", fixture.base.to_str().unwrap()],
    );
    let target = created["result"]["root_pane"]["pane_id"]
        .as_str()
        .unwrap()
        .to_string();

    let resumed = socket_request(
        socket_path,
        serde_json::json!({
            "id": "resume",
            "method": "agent.resume",
            "params": {"name": "again", "from": fixture.pane_id, "pane_id": target, "timeout_ms": 8000}
        }),
    );
    assert_eq!(resumed["result"]["type"], "agent_resumed", "{resumed}");
    assert_eq!(
        resumed["result"]["argv"],
        serde_json::json!(["claude", "--resume", "recorded-1"])
    );
    assert_eq!(resumed["result"]["agent"]["pane_id"], target);
    assert_eq!(
        wait_for_file(&fixture.base.join("claude-args"), Duration::from_secs(8)),
        "--resume\nrecorded-1\n"
    );

    let both = socket_request(
        socket_path,
        serde_json::json!({
            "id": "both",
            "method": "agent.resume",
            "params": {"name": "x", "from": target, "pane_id": target, "cwd": "/tmp"}
        }),
    );
    assert_eq!(both["error"]["code"], "invalid_request");

    cleanup_spawned_herdr(fixture.herdr, fixture.base);
}

#[test]
fn pane_shell_state_reports_a_ready_shell_and_an_agent() {
    let fixture = resume_fixture(&["claude"]);
    let socket_path = &fixture.socket_path;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let state = socket_request(
            socket_path,
            serde_json::json!({"id": "s", "method": "pane.shell_state", "params": {"pane_id": fixture.pane_id}}),
        );
        assert_eq!(state["result"]["type"], "pane_shell_state", "{state}");
        if state["result"]["shell"]["state"] == "ready" {
            break;
        }
        assert!(Instant::now() < deadline, "{state}");
        std::thread::sleep(Duration::from_millis(50));
    }
    run_cli_json(
        socket_path,
        &[
            "agent",
            "start",
            "a",
            "--kind",
            "claude",
            "--pane",
            &fixture.pane_id,
        ],
    );
    let state = socket_request(
        socket_path,
        serde_json::json!({"id": "s", "method": "pane.shell_state", "params": {"pane_id": fixture.pane_id}}),
    );
    assert_eq!(state["result"]["shell"]["state"], "busy");
    let missing = socket_request(
        socket_path,
        serde_json::json!({"id": "s", "method": "pane.shell_state", "params": {"pane_id": "w99:p9"}}),
    );
    assert_eq!(missing["error"]["code"], "pane_not_found");

    cleanup_spawned_herdr(fixture.herdr, fixture.base);
}
