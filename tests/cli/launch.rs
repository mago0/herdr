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
