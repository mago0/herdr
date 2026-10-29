use super::*;
use crate::api::schema::{Method, Request, SubtreeControlParams};
use serde_json::json;

fn control(f: &mut Fixture, action: &str, target: &str, pause_id: &str) -> serde_json::Value {
    let response = f.app.handle_api_request(Request {
        id: "pause-test".into(),
        method: Method::SubtreeControl(SubtreeControlParams {
            expected: Vec::new(),
            action: action.into(),
            target: Some(target.into()),
            pause_id: Some(pause_id.into()),
            authorization: None,
            evidence: None,
            restore_jobs: Vec::new(),
        }),
    });
    serde_json::from_str(&response).unwrap()
}

#[tokio::test]
async fn pause_preserves_nested_links_and_quarantines_queued_automation() {
    let mut f = fixture();
    f.supervise(vec![SuperviseReport::Blocked, SuperviseReport::Exited]);
    f.worker_state(AgentState::Working);
    let queued = f.deliver("worker", "ordinary work");
    let before = f.app.orchestration.links.clone();
    let result = control(&mut f, "pause", "lead", "incident");
    assert_eq!(result["result"]["pause"]["state"], "pausing", "{result}");
    assert_eq!(f.app.orchestration.links, before);
    f.worker_state(AgentState::Idle);
    f.tick();
    let input = typed(&mut f.worker_rx).await;
    assert!(!input.contains("ordinary work"), "{input}");
    assert!(input.contains("checkpoint"), "{input}");
    let result = control(&mut f, "status", "lead", "incident");
    assert_eq!(
        result["result"]["pause"]["members"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert!(result.to_string().contains(&queued.delivery_id));
    assert_eq!(f.app.orchestration.links, before);
}

#[tokio::test]
async fn pause_is_idempotent_and_requires_explicit_resume_authorization() {
    let mut f = fixture();
    f.supervise(vec![SuperviseReport::Blocked]);
    let a = control(&mut f, "pause", "lead", "repeat");
    let b = control(&mut f, "pause", "lead", "repeat");
    assert_eq!(
        a["result"]["pause"]["members"],
        b["result"]["pause"]["members"]
    );
    assert_eq!(
        control(&mut f, "resume", "lead", "repeat")["error"]["code"],
        "authorization_required"
    );
    assert!(f.app.subtree_terminal_paused(&f.terminal(1, f.worker)));
}

#[tokio::test]
async fn raw_supervision_clear_and_prompt_are_rejected_without_touching_links() {
    let mut f = fixture();
    f.supervise(vec![SuperviseReport::Blocked]);
    control(&mut f, "pause", "lead", "raw");
    let before = f.app.orchestration.links.clone();
    for request in [
        serde_json::json!({"id":"raw", "method":"pane.supervise", "params":{"pane_id":"worker","supervisor":null,"report":[]}}),
        serde_json::json!({"id":"raw", "method":"agent.prompt", "params":{"target":"worker","text":"restart work"}}),
        serde_json::json!({"id":"raw", "method":"pane.close", "params":{"pane_id":"worker"}}),
    ] {
        let req: Request = serde_json::from_value(request).unwrap();
        let out: serde_json::Value = serde_json::from_str(&f.app.handle_api_request(req)).unwrap();
        assert_eq!(out["error"]["code"], "subtree_paused", "{out}");
    }
    assert_eq!(f.app.orchestration.links, before);
}

#[tokio::test]
async fn blocked_agent_remains_unconfirmed_and_draft_input_is_untouched() {
    let mut f = fixture();
    f.supervise(vec![SuperviseReport::Blocked]);
    let terminal = f.terminal(1, f.worker);
    f.app
        .note_pane_keys(&terminal, &[deliveries::DraftKey::Text(1)]);
    f.worker_state(AgentState::Blocked);
    control(&mut f, "pause", "lead", "blocked");
    f.tick();
    assert!(f.worker_rx.try_recv().is_err());
    assert!(f.app.orchestration.drafts.holds(&terminal, Instant::now()));
    let status = control(&mut f, "status", "lead", "blocked");
    assert_eq!(status["result"]["pause"]["state"], "pausing");
    assert!(status["result"]["pause"]["pending"]
        .as_array()
        .unwrap()
        .contains(&serde_json::json!("w2:p1")));
}

#[tokio::test]
async fn active_watch_is_suspended_in_place_and_running_output_is_quarantined() {
    let mut f = fixture();
    f.supervise(vec![]);
    f.add_job(job("pr-watch", "echo PR-news"));
    let run = f
        .app
        .orchestration
        .jobs
        .begin("pr-watch", None, Instant::now())
        .unwrap();
    control(&mut f, "pause", "lead", "watch");
    f.app.start_job("pr-watch", None);
    assert_eq!(
        f.app.orchestration.jobs.get("pr-watch").unwrap().running,
        Some(run.run_id)
    );
    f.app
        .handle_job_finished("pr-watch", run.run_id, Some(0), "PR-news");
    assert!(f.app.orchestration.jobs.contains("pr-watch"));
    let status = control(&mut f, "status", "lead", "watch");
    assert!(status.to_string().contains("PR-news"));
    assert!(f.worker_rx.try_recv().is_err());
}

#[tokio::test]
async fn pause_survives_restart_and_fails_verification_on_topology_drift() {
    let mut f = fixture();
    let dir = std::env::temp_dir().join(format!("pause-restart-{}", unique_suffix()));
    f.app.orchestration = Orchestration::new(Some(dir.join(store::STORE_FILE)), 600);
    f.supervise(vec![SuperviseReport::Blocked]);
    control(&mut f, "pause", "lead", "restart");
    f.app.orchestration = Orchestration::new(Some(dir.join(store::STORE_FILE)), 600);
    f.app.ensure_orchestration_loaded();
    assert!(f.app.subtree_terminal_paused(&f.terminal(1, f.worker)));
    let worker = f.terminal(1, f.worker);
    f.app.orchestration.links.remove(&worker);
    let status = control(&mut f, "status", "lead", "restart");
    assert_eq!(status["result"]["pause"]["state"], "failed");
    assert!(!status["result"]["pause"]["problems"]
        .as_array()
        .unwrap()
        .is_empty());
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn save_failure_never_acknowledges_a_successful_pause() {
    let mut f = fixture();
    let dir = std::env::temp_dir().join(format!("pause-unwritable-{}", unique_suffix()));
    std::fs::write(&dir, "not a directory").unwrap();
    f.app.orchestration = Orchestration::new(Some(dir.join(store::STORE_FILE)), 600);
    f.app.orchestration.loaded = true;
    let out = control(&mut f, "pause", "lead", "bad-save");
    assert_eq!(out["error"]["code"], "pause_persist_failed", "{out}");
    assert!(f.app.subtree_terminal_paused(&f.terminal(0, f.supervisor)));
    std::fs::remove_file(dir).unwrap();
}

fn authorized_control(
    f: &mut Fixture,
    action: &str,
    pause_id: &str,
    jobs: &[&str],
) -> serde_json::Value {
    let response = f.app.handle_api_request(Request {
        id: "authorized".into(),
        method: Method::SubtreeControl(SubtreeControlParams {
            expected: Vec::new(),
            action: action.into(),
            target: None,
            pause_id: Some(pause_id.into()),
            authorization: Some(
                "Matt explicitly authorized after ownership/PR reconciliation".into(),
            ),
            evidence: None,
            restore_jobs: jobs.iter().map(|s| s.to_string()).collect(),
        }),
    });
    serde_json::from_str(&response).unwrap()
}

fn checkpoint(f: &mut Fixture, target: &str, pause_id: &str) -> serde_json::Value {
    let response = f.app.handle_api_request(Request {
        id: "checkpoint".into(),
        method: Method::SubtreeControl(SubtreeControlParams {
            expected: Vec::new(),
            action: "checkpoint".into(),
            target: Some(target.into()),
            pause_id: Some(pause_id.into()),
            authorization: None,
            evidence: Some("/checkpoints/owned-work-and-wakes-stopped.md".into()),
            restore_jobs: Vec::new(),
        }),
    });
    serde_json::from_str(&response).unwrap()
}

#[tokio::test]
async fn explicit_resume_restores_only_selected_jobs_and_never_replays_old_lines() {
    let mut f = fixture();
    f.supervise(vec![]);
    f.add_job(job("still-open-pr", "echo news"));
    f.add_job(job("merged-pr", "echo stale"));
    f.worker_state(AgentState::Working);
    let delivery = f.deliver("worker", "stale queued instruction");
    control(&mut f, "pause", "lead", "explicit");
    assert_eq!(
        checkpoint(&mut f, "lead", "explicit")["error"]["code"],
        "descendants_unconfirmed"
    );
    assert!(checkpoint(&mut f, "worker", "explicit")["error"].is_null());
    assert!(checkpoint(&mut f, "lead", "explicit")["error"].is_null());
    let out = authorized_control(&mut f, "resume", "explicit", &["still-open-pr"]);
    assert_eq!(out["result"]["pause"]["state"], "resumed", "{out}");
    assert!(!f.app.subtree_job_suspended("still-open-pr"));
    assert!(f.app.subtree_job_suspended("merged-pr"));
    assert_eq!(f.app.orchestration.jobs.len(), 2);
    assert!(f
        .app
        .orchestration
        .deliveries
        .iter()
        .all(|d| d.id != delivery.delivery_id));
    assert!(out.to_string().contains("stale queued instruction"));
    let out = authorized_control(&mut f, "resume", "explicit", &["still-open-pr"]);
    assert_eq!(out["result"]["pause"]["state"], "resumed");
    assert_eq!(f.app.orchestration.jobs.len(), 2);
}

#[tokio::test]
async fn independently_paused_descendant_remains_fenced_after_parent_resume() {
    let mut f = fixture();
    f.supervise(vec![]);
    control(&mut f, "pause", "worker", "narrow");
    checkpoint(&mut f, "worker", "narrow");
    control(&mut f, "pause", "lead", "broad");
    checkpoint(&mut f, "worker", "broad");
    checkpoint(&mut f, "lead", "broad");
    let out = authorized_control(&mut f, "resume", "broad", &[]);
    assert_eq!(out["result"]["pause"]["state"], "resumed", "{out}");
    assert!(f.app.subtree_terminal_paused(&f.terminal(1, f.worker)));
    assert!(!f.app.subtree_terminal_paused(&f.terminal(0, f.supervisor)));
}

#[tokio::test]
async fn nested_incident_regression_keeps_every_parent_child_edge() {
    let mut f = fixture();
    f.supervise(vec![SuperviseReport::Blocked]);
    f.app
        .state
        .workspaces
        .push(Workspace::test_new("grandchild"));
    f.app.state.ensure_test_terminals();
    let child = f.app.state.workspaces[2].tabs[0].root_pane;
    let (runtime, mut rx) = crate::terminal::TerminalRuntime::test_with_channel(80, 24);
    f.app.state.insert_test_runtime(child, runtime);
    f.set_agent(2, child, "grandchild", AgentState::Idle);
    let out = f.app.handle_pane_supervise(
        "nested".into(),
        PaneSuperviseParams {
            pane_id: "grandchild".into(),
            supervisor: Some("worker".into()),
            report: vec![SuperviseReport::Exited],
        },
    );
    assert!(serde_json::from_str::<SuccessResponse>(&out).is_ok());
    let links = f.app.orchestration.links.clone();
    control(&mut f, "pause", "lead", "nested");
    f.tick();
    assert!(typed(&mut rx).await.contains("checkpoint"));
    assert!(
        f.worker_rx.try_recv().is_err(),
        "supervisor must checkpoint last"
    );
    assert_eq!(
        checkpoint(&mut f, "worker", "nested")["error"]["code"],
        "descendants_unconfirmed"
    );
    checkpoint(&mut f, "grandchild", "nested");
    checkpoint(&mut f, "worker", "nested");
    checkpoint(&mut f, "lead", "nested");
    let status = control(&mut f, "status", "lead", "nested");
    assert_eq!(status["result"]["pause"]["state"], "paused", "{status}");
    assert_eq!(f.app.orchestration.links, links);
}

#[tokio::test]
async fn resumed_unselected_jobs_cannot_bypass_suspension_via_raw_api() {
    let mut f = fixture();
    f.supervise(vec![]);
    f.add_job(job("closed-pr", "echo stale"));
    control(&mut f, "pause", "lead", "job-gate");
    checkpoint(&mut f, "worker", "job-gate");
    checkpoint(&mut f, "lead", "job-gate");
    authorized_control(&mut f, "resume", "job-gate", &[]);
    let response = f.app.handle_api_request(Request {
        id: "raw-job".into(),
        method: Method::JobRun(crate::api::schema::JobTarget {
            name: "closed-pr".into(),
        }),
    });
    assert_eq!(error_code(&response), "subtree_paused");
}

#[tokio::test]
async fn corrupt_pause_journal_is_not_overwritten_to_open_the_gate() {
    let mut f = fixture();
    let dir = std::env::temp_dir().join(format!("pause-corrupt-{}", unique_suffix()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("subtree-pauses.json");
    std::fs::write(&path, "{broken").unwrap();
    f.app.orchestration = Orchestration::new(Some(dir.join(store::STORE_FILE)), 600);
    let out = control(&mut f, "pause", "lead", "do-not-overwrite");
    assert_eq!(out["error"]["code"], "pause_state_unreadable", "{out}");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "{broken");
    assert!(f.app.subtree_terminal_paused(&f.terminal(1, f.worker)));
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn startup_restore_fence_survives_restart_and_corruption_fails_closed() {
    let mut f = fixture();
    let dir = std::env::temp_dir().join(format!("pause-restore-{}", unique_suffix()));
    f.app.orchestration = Orchestration::new(Some(dir.join(store::STORE_FILE)), 600);
    f.supervise(vec![]);
    control(&mut f, "pause", "lead", "restore");
    let path = dir.join("subtree-pauses.json");
    let fence = pause::RestoreFence::load(&path);
    assert!(fence.holds(Some("w1:p1")));
    assert!(fence.holds(Some("w2:p1")));
    assert!(!fence.holds(Some("w3:p1")));
    std::fs::write(&path, "corrupt").unwrap();
    assert!(pause::RestoreFence::load(&path).holds(Some("w3:p1")));
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn identity_replacement_requires_explicit_evidenced_reconciliation() {
    let mut f = fixture();
    f.supervise(vec![]);
    control(&mut f, "pause", "lead", "identity");
    f.set_agent(1, f.worker, "replacement", AgentState::Idle);
    assert_eq!(
        control(&mut f, "status", "lead", "identity")["result"]["pause"]["state"],
        "failed"
    );
    let mut params = SubtreeControlParams {
        expected: Vec::new(),
        action: "reconcile".into(),
        target: Some("replacement".into()),
        pause_id: Some("identity".into()),
        authorization: None,
        evidence: Some("/checkpoint.md".into()),
        restore_jobs: vec![],
    };
    let out = f.app.handle_api_request(Request {
        id: "reconcile".into(),
        method: Method::SubtreeControl(params.clone()),
    });
    assert_eq!(error_code(&out), "authorization_required");
    params.authorization =
        Some("Matt authorized identity replacement after inspecting its checkpoint".into());
    let out = f.app.handle_api_request(Request {
        id: "reconcile".into(),
        method: Method::SubtreeControl(params),
    });
    assert!(
        serde_json::from_str::<SuccessResponse>(&out).is_ok(),
        "{out}"
    );
    checkpoint(&mut f, "lead", "identity");
    assert_eq!(
        authorized_control(&mut f, "resume", "identity", &[])["result"]["pause"]["state"],
        "resumed"
    );
}

#[tokio::test]
async fn closing_an_unpaused_group_parent_cannot_destroy_a_paused_child() {
    for method in ["workspace.close", "pane.close", "tab.close"] {
        let mut f = fixture();
        for (i, ws) in f.app.state.workspaces.iter_mut().enumerate() {
            ws.worktree_space = Some(crate::workspace::WorktreeSpaceMembership {
                key: "repo".into(),
                label: "repo".into(),
                repo_root: "/repo".into(),
                checkout_path: if i == 0 { "/repo" } else { "/repo/child" }.into(),
                is_linked_worktree: i != 0,
            });
        }
        f.supervise(vec![]);
        control(&mut f, "pause", "worker", "group");
        let params = match method {
            "workspace.close" => {
                serde_json::json!({"workspace_id": f.app.public_workspace_id(0), "close_group":true})
            }
            "pane.close" => {
                serde_json::json!({"pane_id":f.app.public_pane_id(0,f.supervisor).unwrap()})
            }
            _ => serde_json::json!({"tab_id":f.app.public_tab_id(0,0).unwrap()}),
        };
        let req: Request = serde_json::from_value(
            serde_json::json!({"id":"group-close","method":method,"params":params}),
        )
        .unwrap();
        let response = f.app.handle_api_request(req);
        assert_eq!(error_code(&response), "subtree_paused", "{method}");
        assert_eq!(f.app.state.workspaces.len(), 2);
    }
}

#[tokio::test]
async fn job_environment_is_preserved_privately_not_exposed_in_pause_receipts() {
    let mut f = fixture();
    f.supervise(vec![]);
    let mut spec = job("private-watch", "echo news");
    spec.env.insert(
        "TEST_CREDENTIAL".into(),
        "private-sentinel-not-a-secret".into(),
    );
    f.add_job(spec);
    let out = control(&mut f, "pause", "lead", "private");
    assert!(!out.to_string().contains("private-sentinel-not-a-secret"));
    assert!(f
        .app
        .orchestration
        .jobs
        .get("private-watch")
        .unwrap()
        .spec
        .env
        .contains_key("TEST_CREDENTIAL"));
}

#[tokio::test]
async fn vanished_descendant_is_unconfirmed_not_silently_omitted() {
    let mut f = fixture();
    f.supervise(vec![]);
    f.app.state.workspaces.remove(1);
    let out = control(&mut f, "pause", "lead", "missing-child");
    assert_eq!(
        out["result"]["pause"]["members"].as_array().unwrap().len(),
        2,
        "{out}"
    );
    assert_eq!(out["result"]["pause"]["state"], "failed", "{out}");
    assert!(out["result"]["pause"]["pending"].as_array().unwrap().len() >= 2);
}

#[tokio::test]
async fn explicit_release_does_not_rearm_wakes_or_claim_an_incomplete_pause_confirmed() {
    let mut f = fixture();
    f.supervise(vec![]);
    f.add_job(job("release-watch", "echo old-work"));
    control(&mut f, "pause", "lead", "release");
    let out = authorized_control(&mut f, "release", "release", &[]);
    assert_eq!(out["result"]["pause"]["state"], "released", "{out}");
    assert!(!out["result"]["pause"]["pending"]
        .as_array()
        .unwrap()
        .is_empty());
    assert!(f.app.subtree_job_suspended("release-watch"));
    assert_eq!(
        authorized_control(&mut f, "resume", "release", &[])["error"]["code"],
        "pause_already_ended"
    );
}

#[tokio::test]
async fn legacy_addressing_cannot_bypass_subtree_pause() {
    let mut f = fixture();
    f.app.state.workspaces[0].test_split(ratatui::layout::Direction::Horizontal);
    f.app.state.workspaces[0].test_add_tab(None);
    f.app.state.ensure_test_terminals();
    f.supervise(vec![]);
    control(&mut f, "pause", "lead", "aliases");
    for (method, params) in [
        (
            "pane.send_text",
            json!({"pane_id":"1-1","text":"continue\r"}),
        ),
        (
            "pane.send_keys",
            json!({"pane_id":format!("p_{}",f.worker.raw()),"keys":["enter"]}),
        ),
        (
            "pane.send_input",
            json!({"pane_id":format!("p_{}_{}",f.app.public_workspace_id(1),f.worker.raw()),"text":"continue","keys":["enter"]}),
        ),
        ("pane.close", json!({"pane_id":"1-1"})),
        ("tab.close", json!({"tab_id":"1:1"})),
        ("workspace.close", json!({"workspace_id":"w_1"})),
    ] {
        let request: Request =
            serde_json::from_value(json!({"id":"alias","method":method,"params":params})).unwrap();
        let out = f.app.handle_api_request(request);
        assert_eq!(error_code(&out), "subtree_paused", "{method}");
    }
}

#[tokio::test]
async fn fleet_owned_child_without_a_link_is_fenced_but_not_confirmed() {
    let mut f = fixture();
    f.add_job(job("owned-watch", "echo news"));
    let root = f.app.public_pane_id(0, f.supervisor).unwrap();
    let worker = f.app.public_pane_id(1, f.worker).unwrap();
    let request:Request=serde_json::from_value(json!({"id":"owned","method":"subtree.control","params":{
        "action":"pause","target":root,"pause_id":"owned","expected":[{"pane_id":worker,"supervisor_pane_id":root,"agent":"worker"}]
    }})).unwrap();
    let out: serde_json::Value =
        serde_json::from_str(&f.app.handle_api_request(request.clone())).unwrap();
    assert!(
        f.app.subtree_terminal_paused(&f.terminal(1, f.worker)),
        "{out}"
    );
    assert!(f.app.subtree_job_suspended("owned-watch"));
    assert!(
        f.app.orchestration.links.is_empty(),
        "pause must not silently repair hierarchy"
    );
    assert_eq!(out["result"]["pause"]["state"], "failed", "{out}");
    assert_eq!(
        out["result"]["pause"]["members"].as_array().unwrap().len(),
        2
    );
    let repeated: serde_json::Value =
        serde_json::from_str(&f.app.handle_api_request(request)).unwrap();
    assert_eq!(
        repeated["result"]["pause"]["members"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let deliver:Request=serde_json::from_value(json!({"id":"raw-owned","method":"agent.deliver","params":{"target":"worker","text":"do not start"}})).unwrap();
    assert_eq!(
        error_code(&f.app.handle_api_request(deliver)),
        "subtree_paused"
    );
}

#[tokio::test]
async fn adding_owned_scope_requires_supervisors_to_checkpoint_again() {
    let mut f = fixture();
    control(&mut f, "pause", "lead", "grow");
    assert_eq!(
        checkpoint(&mut f, "lead", "grow")["result"]["pause"]["state"],
        "paused"
    );
    let root = f.app.public_pane_id(0, f.supervisor).unwrap();
    let worker = f.app.public_pane_id(1, f.worker).unwrap();
    let request:Request=serde_json::from_value(json!({"id":"grow","method":"subtree.control","params":{
        "action":"pause","target":root,"pause_id":"grow","expected":[{"pane_id":worker,"supervisor_pane_id":root,"agent":"worker"}]
    }})).unwrap();
    let out: serde_json::Value = serde_json::from_str(&f.app.handle_api_request(request)).unwrap();
    let root_member = out["result"]["pause"]["members"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["pane_id"] == root)
        .unwrap();
    assert!(root_member["checkpoint"].is_null(), "{out}");
    assert!(!root_member["requested"].as_bool().unwrap());
}
