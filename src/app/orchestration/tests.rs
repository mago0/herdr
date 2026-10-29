use std::time::{Duration, Instant};

use bytes::Bytes;

use super::*;
use crate::api::schema::{ErrorResponse, SuccessResponse};
use crate::app::Mode;
use crate::config::Config;
use crate::detect::Agent;
use crate::events::AppEvent;
use crate::layout::PaneId;
use crate::workspace::Workspace;

/// Two workspaces with one agent pane each: a supervisor in the first, a worker in the second.
struct Fixture {
    app: App,
    supervisor: PaneId,
    worker: PaneId,
    supervisor_rx: tokio::sync::mpsc::Receiver<Bytes>,
    worker_rx: tokio::sync::mpsc::Receiver<Bytes>,
}

fn fixture() -> Fixture {
    let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(
        &Config::default(),
        crate::app::AppPolicy::TEST,
        None,
        api_rx,
        crate::api::EventHub::default(),
    );
    app.state.workspaces = vec![Workspace::test_new("lead"), Workspace::test_new("work")];
    app.state.ensure_test_terminals();
    app.state.active = Some(0);
    app.state.selected = 0;
    app.state.mode = Mode::Terminal;
    let supervisor = app.state.workspaces[0].tabs[0].root_pane;
    let worker = app.state.workspaces[1].tabs[0].root_pane;
    let (supervisor_runtime, supervisor_rx) =
        crate::terminal::TerminalRuntime::test_with_channel(80, 24);
    let (worker_runtime, worker_rx) = crate::terminal::TerminalRuntime::test_with_channel(80, 24);
    app.state
        .insert_test_runtime(supervisor, supervisor_runtime);
    app.state.insert_test_runtime(worker, worker_runtime);
    let mut fixture = Fixture {
        app,
        supervisor,
        worker,
        supervisor_rx,
        worker_rx,
    };
    fixture.set_agent(0, supervisor, "lead", AgentState::Idle);
    fixture.set_agent(1, worker, "worker", AgentState::Idle);
    fixture
}

impl Fixture {
    fn terminal(&self, ws_idx: usize, pane: PaneId) -> TerminalId {
        self.app.state.workspaces[ws_idx]
            .terminal_id(pane)
            .cloned()
            .unwrap()
    }

    fn set_agent(&mut self, ws_idx: usize, pane: PaneId, name: &str, state: AgentState) {
        let terminal = self.terminal(ws_idx, pane);
        let terminal = self.app.state.terminals.get_mut(&terminal).unwrap();
        terminal.set_agent_name(name.into());
        terminal.set_detected_state(Some(Agent::Claude), state);
    }

    /// Moves the worker's agent to a state the way the screen detector does.
    fn worker_state(&mut self, state: AgentState) {
        self.app.handle_internal_event(AppEvent::StateChanged {
            pane_id: self.worker,
            agent: Some(Agent::Claude),
            state,
            visible_blocker: state == AgentState::Blocked,
            visible_working: state == AgentState::Working,
            process_exited: false,
            observed_at: Instant::now(),
        });
    }

    fn tick(&mut self) {
        self.app.orchestration.wake();
        self.app.run_orchestration_tick(Instant::now());
    }

    fn deliver(&mut self, target: &str, text: &str) -> DeliveryInfo {
        let response = self.app.handle_agent_deliver(
            "req".into(),
            AgentDeliverParams {
                target: target.into(),
                text: text.into(),
                source: None,
                ttl_ms: None,
            },
        );
        match serde_json::from_str::<SuccessResponse>(&response)
            .unwrap_or_else(|_| panic!("deliver failed: {response}"))
            .result
        {
            ResponseResult::Delivery { delivery } => delivery,
            other => panic!("unexpected result: {other:?}"),
        }
    }

    fn supervise(&mut self, report: Vec<SuperviseReport>) {
        let response = self.app.handle_pane_supervise(
            "req".into(),
            PaneSuperviseParams {
                pane_id: "worker".into(),
                supervisor: Some("lead".into()),
                report,
            },
        );
        assert!(
            serde_json::from_str::<SuccessResponse>(&response).is_ok(),
            "{response}"
        );
    }

    fn agent(&self, ws_idx: usize, pane: PaneId) -> crate::api::schema::AgentInfo {
        self.app.agent_info(ws_idx, pane).unwrap()
    }
}

/// Everything typed into a pane, as text. The writer runs beside the test, and a submission gets
/// its Enter a moment after its text, so this waits a short time for both.
async fn typed(rx: &mut tokio::sync::mpsc::Receiver<Bytes>) -> String {
    let mut bytes = Vec::new();
    if let Ok(Some(chunk)) = tokio::time::timeout(Duration::from_millis(400), rx.recv()).await {
        bytes.extend_from_slice(&chunk);
    }
    while !bytes.is_empty() && !bytes.ends_with(b"\r") {
        match tokio::time::timeout(Duration::from_secs(2), rx.recv()).await {
            Ok(Some(chunk)) => bytes.extend_from_slice(&chunk),
            _ => break,
        }
    }
    while let Ok(chunk) = rx.try_recv() {
        bytes.extend_from_slice(&chunk);
    }
    // The writer reports completion just after the Enter is handed over.
    tokio::time::sleep(Duration::from_millis(20)).await;
    String::from_utf8_lossy(&bytes).into_owned()
}

fn unique_suffix() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0)
}

fn error_code(response: &str) -> String {
    serde_json::from_str::<ErrorResponse>(response)
        .unwrap_or_else(|_| panic!("expected an error: {response}"))
        .error
        .code
}

#[tokio::test]
async fn a_delivery_to_a_pane_that_is_not_focused_is_typed_at_once() {
    let mut f = fixture();
    let delivery = f.deliver("worker", "MAIL|1|status");
    assert_eq!(delivery.state, DeliveryState::Delivered);
    assert_eq!(typed(&mut f.worker_rx).await, "MAIL|1|status\r");
    f.tick();
    assert_eq!(f.agent(1, f.worker).pending_deliveries, None);
}

#[tokio::test]
async fn a_delivery_to_the_focused_pane_waits_until_focus_moves_away() {
    let mut f = fixture();
    let delivery = f.deliver("lead", "MAIL|1|question");
    assert_eq!(delivery.state, DeliveryState::Queued);
    f.tick();
    assert_eq!(typed(&mut f.supervisor_rx).await, "");
    assert_eq!(f.agent(0, f.supervisor).pending_deliveries, Some(1));

    f.app.state.active = Some(1);
    f.tick();
    assert_eq!(typed(&mut f.supervisor_rx).await, "MAIL|1|question\r");
}

#[tokio::test]
async fn focus_does_not_hold_a_delivery_when_no_client_is_attached() {
    let mut f = fixture();
    f.app.orchestration.client_attached = false;
    assert_eq!(f.deliver("lead", "wake").state, DeliveryState::Delivered);
    assert_eq!(typed(&mut f.supervisor_rx).await, "wake\r");
}

#[tokio::test]
async fn a_delivery_to_a_blocked_agent_waits_until_it_is_not_blocked() {
    let mut f = fixture();
    f.worker_state(AgentState::Blocked);
    assert_eq!(f.deliver("worker", "continue").state, DeliveryState::Queued);
    f.tick();
    assert_eq!(typed(&mut f.worker_rx).await, "");

    f.worker_state(AgentState::Idle);
    f.app.run_orchestration_tick(Instant::now());
    assert_eq!(typed(&mut f.worker_rx).await, "continue\r");
}

#[tokio::test]
async fn deliveries_to_one_pane_are_typed_one_at_a_time_in_order() {
    let mut f = fixture();
    f.worker_state(AgentState::Blocked);
    f.deliver("worker", "first");
    f.deliver("worker", "second");
    f.worker_state(AgentState::Idle);

    f.tick();
    assert_eq!(typed(&mut f.worker_rx).await, "first\r");
    f.tick();
    assert_eq!(typed(&mut f.worker_rx).await, "second\r");
    f.tick();
    assert_eq!(typed(&mut f.worker_rx).await, "");
    assert_eq!(f.agent(1, f.worker).pending_deliveries, None);
}

#[tokio::test]
async fn a_second_delivery_waits_while_the_first_is_being_written() {
    let mut f = fixture();
    // No await between the two: the first has its text out and its Enter still to come.
    assert_eq!(f.deliver("worker", "first").state, DeliveryState::Delivered);
    assert_eq!(f.deliver("worker", "second").state, DeliveryState::Queued);
    f.tick();
    assert_eq!(typed(&mut f.worker_rx).await, "first\r");
    f.tick();
    assert_eq!(typed(&mut f.worker_rx).await, "second\r");
}

#[tokio::test]
async fn delivery_text_is_typed_with_its_control_characters_removed() {
    let mut f = fixture();
    let delivery = f.deliver("worker", "note\x1b[201~\r\x03 end");
    assert_eq!(delivery.text, "note[201~ end");
    assert_eq!(typed(&mut f.worker_rx).await, "note[201~ end\r");

    let only_controls = f.app.handle_agent_deliver(
        "req".into(),
        AgentDeliverParams {
            target: "worker".into(),
            text: "\x1b\r".into(),
            source: None,
            ttl_ms: None,
        },
    );
    assert_eq!(error_code(&only_controls), "empty_delivery");
}

#[tokio::test]
async fn a_delivery_waits_while_the_agent_works_and_is_typed_when_it_waits_for_input() {
    let mut f = fixture();
    f.worker_state(AgentState::Working);
    assert_eq!(f.deliver("worker", "wake").state, DeliveryState::Queued);
    f.tick();
    assert_eq!(typed(&mut f.worker_rx).await, "");

    f.worker_state(AgentState::Idle);
    f.app.run_orchestration_tick(Instant::now());
    assert_eq!(typed(&mut f.worker_rx).await, "wake\r");
}

#[tokio::test]
async fn a_delivery_waits_while_the_pane_holds_text_that_was_typed_and_not_sent() {
    let mut f = fixture();
    let terminal = f.terminal(1, f.worker);
    f.app
        .note_pane_keys(&terminal, &[deliveries::DraftKey::Text(2)]);
    assert_eq!(f.deliver("worker", "wake").state, DeliveryState::Queued);
    f.tick();
    assert_eq!(typed(&mut f.worker_rx).await, "");

    // One of two characters erased leaves text.
    f.app
        .note_pane_keys(&terminal, &[deliveries::DraftKey::Erase]);
    f.tick();
    assert_eq!(typed(&mut f.worker_rx).await, "");

    // The release needs no tick from the timer: the key that clears the text asks for one.
    f.app
        .note_pane_keys(&terminal, &[deliveries::DraftKey::Clear]);
    f.app.run_orchestration_tick(Instant::now());
    assert_eq!(typed(&mut f.worker_rx).await, "wake\r");
}

#[tokio::test]
async fn a_pane_with_a_full_queue_refuses_a_delivery() {
    let mut f = fixture();
    f.worker_state(AgentState::Working);
    for _ in 0..deliveries::MAX_PENDING_PER_TERMINAL {
        f.deliver("worker", "held");
    }
    let refused = f.app.handle_agent_deliver(
        "req".into(),
        AgentDeliverParams {
            target: "worker".into(),
            text: "one more".into(),
            source: None,
            ttl_ms: None,
        },
    );
    assert_eq!(error_code(&refused), "delivery_queue_full");
}

#[tokio::test]
async fn a_delivery_waits_in_a_pane_that_has_no_agent() {
    let mut f = fixture();
    let terminal = f.terminal(1, f.worker);
    let pane_id = f.app.public_pane_id(1, f.worker).unwrap();
    *f.app.state.terminals.get_mut(&terminal).unwrap() =
        crate::terminal::TerminalState::new(terminal.clone(), "/".into());

    assert_eq!(f.deliver(&pane_id, "hello").state, DeliveryState::Queued);
    f.tick();
    assert_eq!(typed(&mut f.worker_rx).await, "");
}

#[tokio::test]
async fn deliveries_of_a_closed_pane_are_dropped() {
    let mut f = fixture();
    f.deliver("lead", "held");
    f.app.state.workspaces.remove(0);
    f.app.state.active = Some(0);
    f.tick();
    assert_eq!(f.app.orchestration.deliveries.iter().count(), 0);
}

#[tokio::test]
async fn a_delivery_past_its_time_limit_is_dropped_with_an_event_and_stays_dropped_after_a_reload()
{
    let dir = std::env::temp_dir().join(format!(
        "herdr-orchestration-ttl-{}-{}",
        std::process::id(),
        unique_suffix()
    ));
    let path = dir.join(store::STORE_FILE);
    let mut f = fixture();
    f.app.orchestration = Orchestration::new(Some(path.clone()), 600);
    let held = f.app.handle_agent_deliver(
        "req".into(),
        AgentDeliverParams {
            target: "lead".into(),
            text: "short lived".into(),
            source: None,
            ttl_ms: Some(40),
        },
    );
    assert!(serde_json::from_str::<SuccessResponse>(&held).is_ok());
    f.app.save_orchestration_now();

    // The time limit is kept in the file, so a restart does not make the delivery last forever.
    f.app.orchestration = Orchestration::new(Some(path), 600);
    f.app.ensure_orchestration_loaded();
    assert_eq!(f.agent(0, f.supervisor).pending_deliveries, Some(1));
    tokio::time::sleep(Duration::from_millis(60)).await;
    let before = f.app.event_hub.current_sequence();
    f.tick();
    assert_eq!(f.agent(0, f.supervisor).pending_deliveries, None);
    let pending = f
        .app
        .event_hub
        .events_after(before)
        .into_iter()
        .filter_map(|(_, event)| match event.data {
            EventData::PaneDeliveryChanged { pending, .. } => Some(pending),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(pending, vec![0]);
    f.app.state.active = Some(1);
    f.tick();
    assert_eq!(typed(&mut f.supervisor_rx).await, "");
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn a_queued_delivery_can_be_listed_and_cancelled() {
    let mut f = fixture();
    let delivery = f.deliver("lead", "held");
    let listed = f.app.handle_agent_deliveries(
        "req".into(),
        AgentDeliveriesParams {
            target: Some("lead".into()),
        },
    );
    let ResponseResult::DeliveryList { deliveries } =
        serde_json::from_str::<SuccessResponse>(&listed)
            .unwrap()
            .result
    else {
        panic!("{listed}");
    };
    assert_eq!(deliveries, vec![delivery.clone()]);

    let cancel = |f: &mut Fixture| {
        f.app.handle_agent_cancel_delivery(
            "req".into(),
            AgentCancelDeliveryParams {
                delivery_id: delivery.delivery_id.clone(),
            },
        )
    };
    assert!(serde_json::from_str::<SuccessResponse>(&cancel(&mut f)).is_ok());
    assert_eq!(error_code(&cancel(&mut f)), "delivery_not_found");
    f.app.state.active = Some(1);
    f.tick();
    assert_eq!(typed(&mut f.supervisor_rx).await, "");
}

#[tokio::test]
async fn empty_and_unknown_deliveries_are_refused() {
    let mut f = fixture();
    let deliver = |f: &mut Fixture, target: &str, text: &str| {
        f.app.handle_agent_deliver(
            "req".into(),
            AgentDeliverParams {
                target: target.into(),
                text: text.into(),
                source: None,
                ttl_ms: None,
            },
        )
    };
    assert_eq!(
        error_code(&deliver(&mut f, "worker", "  \n")),
        "empty_delivery"
    );
    assert_eq!(
        error_code(&deliver(&mut f, "nobody", "hi")),
        "pane_not_found"
    );
    assert_eq!(
        error_code(&deliver(
            &mut f,
            "worker",
            &"x".repeat(MAX_DELIVERY_BYTES + 1)
        )),
        "delivery_too_large"
    );
}

/// One `pane.supervise_report` event: kind, pane, supervisor pane, agent and detail.
type SuperviseReportEvent = (SuperviseReport, String, String, String, Option<String>);

impl Fixture {
    fn supervise_report_events(&self) -> Vec<SuperviseReportEvent> {
        self.app
            .event_hub
            .events_after(0)
            .into_iter()
            .filter_map(|(_, event)| match (event.event, event.data) {
                (
                    EventKind::PaneSuperviseReport,
                    EventData::PaneSuperviseReport {
                        pane_id,
                        supervisor_pane_id,
                        agent,
                        kind,
                        detail,
                    },
                ) => Some((kind, pane_id, supervisor_pane_id, agent, detail)),
                _ => None,
            })
            .collect()
    }
}

#[tokio::test]
async fn a_supervisor_is_told_once_when_its_worker_blocks_with_what_the_worker_shows() {
    let mut f = fixture();
    f.app.state.active = Some(1);
    f.supervise(vec![SuperviseReport::Blocked]);
    let worker_pane = f.app.public_pane_id(1, f.worker).unwrap();
    f.app
        .lookup_runtime_sender(1, f.worker)
        .unwrap()
        .test_process_pty_bytes(b"Do you want to run this command?\r\n  1. Yes\r\n  2. No\r\n");

    f.worker_state(AgentState::Blocked);
    f.tick();
    assert_eq!(
        typed(&mut f.supervisor_rx).await,
        format!(
            "HERDR|blocked|worker|{worker_pane}|Do you want to run this command? / 1. Yes / 2. No\r"
        )
    );
    assert_eq!(
        f.agent(1, f.worker).blocker.as_deref(),
        Some("Do you want to run this command? / 1. Yes / 2. No")
    );
    let lead_pane = f.app.public_pane_id(0, f.supervisor).unwrap();
    let blocked = (
        SuperviseReport::Blocked,
        worker_pane.clone(),
        lead_pane,
        "worker".to_owned(),
        Some("Do you want to run this command? / 1. Yes / 2. No".to_owned()),
    );
    assert_eq!(f.supervise_report_events(), vec![blocked.clone()]);

    f.worker_state(AgentState::Blocked);
    f.tick();
    assert_eq!(typed(&mut f.supervisor_rx).await, "");
    assert_eq!(f.supervise_report_events(), vec![blocked]);

    f.worker_state(AgentState::Working);
    assert_eq!(f.agent(1, f.worker).blocker, None);
}

#[tokio::test]
async fn a_worker_that_is_blocked_when_the_link_is_made_is_reported_at_once() {
    let mut f = fixture();
    f.app.state.active = Some(1);
    let worker_pane = f.app.public_pane_id(1, f.worker).unwrap();
    f.app
        .lookup_runtime_sender(1, f.worker)
        .unwrap()
        .test_process_pty_bytes(b"Trust this folder?\r\n  1. Yes\r\n  2. No\r\n");
    f.worker_state(AgentState::Blocked);
    assert_eq!(typed(&mut f.supervisor_rx).await, "");

    f.supervise(vec![SuperviseReport::Blocked]);
    assert_eq!(
        typed(&mut f.supervisor_rx).await,
        format!("HERDR|blocked|worker|{worker_pane}|Trust this folder? / 1. Yes / 2. No\r")
    );
    // Setting the same link again is not a new block.
    f.tick();
    f.supervise(vec![SuperviseReport::Blocked]);
    assert_eq!(typed(&mut f.supervisor_rx).await, "");
}

#[tokio::test]
async fn a_blocked_worker_is_not_reported_when_the_link_does_not_ask_for_it() {
    let mut f = fixture();
    f.app.state.active = Some(1);
    f.supervise(vec![SuperviseReport::Exited]);
    f.worker_state(AgentState::Blocked);
    f.tick();
    assert_eq!(typed(&mut f.supervisor_rx).await, "");
}

#[tokio::test]
async fn a_supervisor_is_told_when_the_pane_of_its_worker_closes() {
    let mut f = fixture();
    f.app.state.active = Some(1);
    f.supervise(vec![SuperviseReport::Exited]);
    let worker_pane = f.app.public_pane_id(1, f.worker).unwrap();
    f.tick();

    let lead_pane = f.app.public_pane_id(0, f.supervisor).unwrap();
    f.app.state.workspaces.remove(1);
    f.app.state.active = None;
    f.tick();
    assert_eq!(
        typed(&mut f.supervisor_rx).await,
        format!("HERDR|exited|worker|{worker_pane}\r")
    );
    assert_eq!(
        f.supervise_report_events(),
        vec![(
            SuperviseReport::Exited,
            worker_pane.clone(),
            lead_pane,
            "worker".to_owned(),
            None,
        )]
    );
    assert!(f.app.orchestration.links.is_empty());
    f.tick();
    assert_eq!(typed(&mut f.supervisor_rx).await, "");
}

#[tokio::test]
async fn a_supervisor_is_told_once_when_the_agent_of_its_worker_leaves_the_pane() {
    let mut f = fixture();
    f.app.state.active = Some(1);
    f.supervise(vec![SuperviseReport::Exited]);
    let worker_pane = f.app.public_pane_id(1, f.worker).unwrap();
    f.tick();

    let terminal = f.terminal(1, f.worker);
    *f.app.state.terminals.get_mut(&terminal).unwrap() =
        crate::terminal::TerminalState::new(terminal.clone(), "/".into());
    f.tick();
    assert_eq!(
        typed(&mut f.supervisor_rx).await,
        format!("HERDR|exited|worker|{worker_pane}\r")
    );
    f.tick();
    assert_eq!(typed(&mut f.supervisor_rx).await, "");
    assert_eq!(f.app.orchestration.links.len(), 1);
}

#[tokio::test]
async fn a_link_shows_on_the_agent_and_can_be_cleared() {
    let mut f = fixture();
    f.supervise(Vec::new());
    assert_eq!(
        f.agent(1, f.worker).supervisor_pane_id,
        f.app.public_pane_id(0, f.supervisor)
    );
    assert_eq!(f.agent(0, f.supervisor).supervisor_pane_id, None);

    let response = f.app.handle_pane_supervise(
        "req".into(),
        PaneSuperviseParams {
            pane_id: "worker".into(),
            supervisor: None,
            report: Vec::new(),
        },
    );
    assert!(serde_json::from_str::<SuccessResponse>(&response).is_ok());
    assert_eq!(f.agent(1, f.worker).supervisor_pane_id, None);
}

#[tokio::test]
async fn a_link_that_closes_a_loop_or_names_a_missing_pane_is_refused() {
    let mut f = fixture();
    f.supervise(Vec::new());
    let supervise = |f: &mut Fixture, pane: &str, by: &str| {
        f.app.handle_pane_supervise(
            "req".into(),
            PaneSuperviseParams {
                pane_id: pane.into(),
                supervisor: Some(by.into()),
                report: Vec::new(),
            },
        )
    };
    assert_eq!(
        error_code(&supervise(&mut f, "lead", "worker")),
        "supervision_cycle"
    );
    assert_eq!(
        error_code(&supervise(&mut f, "lead", "lead")),
        "supervision_cycle"
    );
    assert_eq!(
        error_code(&supervise(&mut f, "lead", "nobody")),
        "pane_not_found"
    );
    assert_eq!(
        error_code(&supervise(&mut f, "nobody", "lead")),
        "pane_not_found"
    );
}

#[tokio::test]
async fn a_link_is_dropped_when_the_supervisor_pane_closes() {
    let mut f = fixture();
    f.supervise(vec![SuperviseReport::Blocked]);
    f.app.state.workspaces.remove(0);
    f.app.state.active = Some(0);
    f.tick();
    assert!(f.app.orchestration.links.is_empty());
}

/// The fixture with a third agent, `mid`, in a third workspace. `mid` supervises `worker` with
/// `report`, and `lead` supervises `mid` when `lead_over_mid` is true. The worker's workspace is
/// in view, so lines to `lead` are typed at once.
fn chain(
    report: Vec<SuperviseReport>,
    lead_over_mid: bool,
) -> (Fixture, PaneId, tokio::sync::mpsc::Receiver<Bytes>) {
    chain_with_store(report, lead_over_mid, None)
}

fn chain_with_store(
    report: Vec<SuperviseReport>,
    lead_over_mid: bool,
    store_path: Option<std::path::PathBuf>,
) -> (Fixture, PaneId, tokio::sync::mpsc::Receiver<Bytes>) {
    let mut f = fixture();
    if store_path.is_some() {
        f.app.orchestration = Orchestration::new(store_path, 600);
    }
    f.app.state.workspaces.push(Workspace::test_new("mid"));
    f.app.state.ensure_test_terminals();
    let mid = f.app.state.workspaces[2].tabs[0].root_pane;
    let (runtime, mid_rx) = crate::terminal::TerminalRuntime::test_with_channel(80, 24);
    f.app.state.insert_test_runtime(mid, runtime);
    f.set_agent(2, mid, "mid", AgentState::Idle);
    f.app.state.active = Some(1);
    let mut supervise = |pane: &str, by: &str, report: Vec<SuperviseReport>| {
        let response = f.app.handle_pane_supervise(
            "req".into(),
            PaneSuperviseParams {
                pane_id: pane.into(),
                supervisor: Some(by.into()),
                report,
            },
        );
        assert!(
            serde_json::from_str::<SuccessResponse>(&response).is_ok(),
            "{response}"
        );
    };
    if lead_over_mid {
        supervise("mid", "lead", Vec::new());
    }
    supervise("worker", "mid", report);
    f.tick();
    (f, mid, mid_rx)
}

#[tokio::test]
async fn a_pane_whose_supervisor_closes_moves_to_the_supervisor_above_it() {
    let (mut f, mid, _mid_rx) = chain(vec![SuperviseReport::Blocked], true);
    let worker_pane = f.app.public_pane_id(1, f.worker).unwrap();
    let mid_pane = f.app.public_pane_id(2, mid).unwrap();
    let lead_pane = f.app.public_pane_id(0, f.supervisor).unwrap();

    f.app.state.workspaces.remove(2);
    f.tick();
    assert_eq!(
        typed(&mut f.supervisor_rx).await,
        format!("HERDR|orphaned|worker|{worker_pane}|{mid_pane}\r")
    );
    assert_eq!(
        f.supervise_report_events(),
        vec![(
            SuperviseReport::Orphaned,
            worker_pane.clone(),
            lead_pane.clone(),
            "worker".to_owned(),
            Some(mid_pane),
        )]
    );
    assert_eq!(
        f.agent(1, f.worker).supervisor_pane_id,
        Some(lead_pane.clone())
    );
    let link = &f.app.orchestration.links[&f.terminal(1, f.worker)];
    assert_eq!(link.report, vec![SuperviseReport::Blocked]);

    // The move is reported one time, and the moved link reports to its new supervisor.
    f.tick();
    assert_eq!(typed(&mut f.supervisor_rx).await, "");
    f.worker_state(AgentState::Blocked);
    f.tick();
    assert!(typed(&mut f.supervisor_rx)
        .await
        .starts_with(&format!("HERDR|blocked|worker|{worker_pane}")));
    assert_eq!(f.supervise_report_events().len(), 2);
}

#[tokio::test]
async fn a_pane_that_waits_when_it_moves_is_reported_blocked_to_its_new_supervisor() {
    let (mut f, mid, mut mid_rx) = chain(vec![SuperviseReport::Blocked], true);
    let worker_pane = f.app.public_pane_id(1, f.worker).unwrap();
    let mid_pane = f.app.public_pane_id(2, mid).unwrap();
    f.worker_state(AgentState::Blocked);
    f.tick();
    assert!(typed(&mut mid_rx).await.starts_with("HERDR|blocked|worker"));

    f.app.state.workspaces.remove(2);
    f.tick();
    assert_eq!(
        typed(&mut f.supervisor_rx).await,
        format!("HERDR|orphaned|worker|{worker_pane}|{mid_pane}\r")
    );
    f.tick();
    assert!(typed(&mut f.supervisor_rx)
        .await
        .starts_with(&format!("HERDR|blocked|worker|{worker_pane}")));
    let kinds: Vec<SuperviseReport> = f
        .supervise_report_events()
        .into_iter()
        .map(|(kind, ..)| kind)
        .collect();
    assert_eq!(
        kinds,
        vec![
            SuperviseReport::Blocked,
            SuperviseReport::Orphaned,
            SuperviseReport::Blocked
        ]
    );
}

#[tokio::test]
async fn a_pane_with_no_live_supervisor_above_loses_its_link() {
    let (mut f, _mid, _mid_rx) = chain(vec![SuperviseReport::Blocked], false);
    f.app.state.workspaces.remove(2);
    f.tick();
    assert!(f.app.orchestration.links.is_empty());
    assert_eq!(f.agent(1, f.worker).supervisor_pane_id, None);
    assert_eq!(typed(&mut f.supervisor_rx).await, "");
    assert!(f.supervise_report_events().is_empty());
}

#[tokio::test]
async fn a_link_cannot_ask_for_the_orphaned_report() {
    let mut f = fixture();
    let response = f.app.handle_pane_supervise(
        "req".into(),
        PaneSuperviseParams {
            pane_id: "worker".into(),
            supervisor: Some("lead".into()),
            report: vec![SuperviseReport::Orphaned],
        },
    );
    assert_eq!(error_code(&response), "invalid_report");
    assert!(f.app.orchestration.links.is_empty());
}

#[tokio::test]
async fn a_moved_link_comes_back_from_the_state_file() {
    let dir = std::env::temp_dir().join(format!(
        "herdr-orchestration-orphan-{}-{}",
        std::process::id(),
        unique_suffix()
    ));
    let path = dir.join(store::STORE_FILE);
    let (mut f, mid, _mid_rx) =
        chain_with_store(vec![SuperviseReport::Exited], true, Some(path.clone()));
    let worker_pane = f.app.public_pane_id(1, f.worker).unwrap();
    let mid_pane = f.app.public_pane_id(2, mid).unwrap();
    let lead_pane = f.app.public_pane_id(0, f.supervisor).unwrap();

    // The supervisor closes and the server stops before a tick sees it.
    f.app.state.workspaces.remove(2);
    f.app.close_orchestration();
    assert_eq!(
        typed(&mut f.supervisor_rx).await,
        "",
        "a line queued while the server stops is typed by the next server"
    );
    let stored = store::load(&path);
    assert_eq!(stored.links.len(), 1);
    assert_eq!(stored.links[0].pane_id, worker_pane);
    assert_eq!(stored.links[0].supervisor_pane_id, lead_pane);
    assert_eq!(stored.links[0].report, vec![SuperviseReport::Exited]);
    assert_eq!(
        stored
            .deliveries
            .iter()
            .map(|delivery| delivery.text.as_str())
            .collect::<Vec<_>>(),
        vec![format!("HERDR|orphaned|worker|{worker_pane}|{mid_pane}").as_str()]
    );

    f.app.orchestration = Orchestration::new(Some(path.clone()), 600);
    f.app.ensure_orchestration_loaded();
    assert_eq!(f.agent(1, f.worker).supervisor_pane_id, Some(lead_pane));
    f.tick();
    assert_eq!(
        typed(&mut f.supervisor_rx).await,
        format!("HERDR|orphaned|worker|{worker_pane}|{mid_pane}\r")
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn a_working_agent_with_a_still_screen_is_marked_stalled_and_reported() {
    let mut f = fixture();
    f.app.state.active = Some(1);
    f.supervise(vec![SuperviseReport::Stalled]);
    f.app.orchestration.stall_after = Some(Duration::from_secs(600));
    let worker_pane = f.app.public_pane_id(1, f.worker).unwrap();
    f.worker_state(AgentState::Working);

    let start = Instant::now();
    f.app.orchestration.wake();
    f.app.run_orchestration_tick(start);
    assert_eq!(f.agent(1, f.worker).stalled_secs, None);

    f.app.orchestration.wake();
    f.app
        .run_orchestration_tick(start + Duration::from_secs(600));
    f.tick();
    assert_eq!(
        typed(&mut f.supervisor_rx).await,
        format!("HERDR|stalled|worker|{worker_pane}|600\r")
    );
    let stalled = f
        .app
        .event_hub
        .events_after(0)
        .into_iter()
        .filter_map(|(_, event)| match event.data {
            EventData::PaneAgentStalled {
                stalled,
                quiet_secs,
                ..
            } => Some((stalled, quiet_secs)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(stalled, vec![(true, 600)]);
    assert_eq!(
        f.supervise_report_events(),
        vec![(
            SuperviseReport::Stalled,
            worker_pane.clone(),
            f.app.public_pane_id(0, f.supervisor).unwrap(),
            "worker".to_owned(),
            Some("600".to_owned()),
        )]
    );

    f.app
        .lookup_runtime_sender(1, f.worker)
        .unwrap()
        .test_process_pty_bytes(b"progress\r\n");
    f.app.orchestration.wake();
    f.app
        .run_orchestration_tick(start + Duration::from_secs(601));
    assert_eq!(f.agent(1, f.worker).stalled_secs, None);
}

#[tokio::test]
async fn queued_deliveries_and_links_come_back_from_the_state_file() {
    let dir = std::env::temp_dir().join(format!(
        "herdr-orchestration-app-{}-{}",
        std::process::id(),
        unique_suffix()
    ));
    let path = dir.join(store::STORE_FILE);
    let mut f = fixture();
    f.app.orchestration = Orchestration::new(Some(path.clone()), 600);
    f.supervise(vec![SuperviseReport::Blocked, SuperviseReport::Exited]);
    f.deliver("lead", "held while focused");
    f.app.save_orchestration_now();
    assert!(path.exists());

    f.app.orchestration = Orchestration::new(Some(path.clone()), 600);
    f.app.ensure_orchestration_loaded();
    assert_eq!(f.agent(0, f.supervisor).pending_deliveries, Some(1));
    assert_eq!(
        f.agent(1, f.worker).supervisor_pane_id,
        f.app.public_pane_id(0, f.supervisor)
    );
    let link = &f.app.orchestration.links[&f.terminal(1, f.worker)];
    assert_eq!(
        link.report,
        vec![SuperviseReport::Blocked, SuperviseReport::Exited]
    );

    f.app.state.active = Some(1);
    f.tick();
    assert_eq!(typed(&mut f.supervisor_rx).await, "held while focused\r");
    f.tick();
    f.app.save_orchestration_now();
    assert!(
        store::load(&path).deliveries.is_empty(),
        "a delivery that was typed must not be typed again after a restart"
    );
    let _ = std::fs::remove_dir_all(dir);
}

/// Starts typing a line to the worker and returns the state file the next server reads.
fn worker_delivery_in_flight(name: &str) -> (Fixture, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!(
        "herdr-orchestration-{name}-{}-{}",
        std::process::id(),
        unique_suffix()
    ));
    let path = dir.join(store::STORE_FILE);
    let mut f = fixture();
    f.app.orchestration = Orchestration::new(Some(path.clone()), 600);
    f.deliver("worker", "MAIL|1");
    f.tick();
    assert_eq!(f.app.orchestration.in_flight.len(), 1);
    (f, path)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_handoff_lets_a_line_being_typed_finish_so_it_is_not_typed_again() {
    let (mut f, path) = worker_delivery_in_flight("handoff");
    f.app.finish_typing_deliveries(Duration::from_secs(2));
    f.app.close_orchestration();
    assert!(f.app.orchestration.in_flight.is_empty());
    assert!(
        store::load(&path).deliveries.is_empty(),
        "the panes outlive a handoff, so the next server must not type the line again"
    );
    assert_eq!(typed(&mut f.worker_rx).await, "MAIL|1\r");
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

#[tokio::test]
async fn a_shutdown_keeps_a_line_being_typed_for_the_next_server() {
    let (mut f, path) = worker_delivery_in_flight("shutdown");
    f.app.close_orchestration();
    let stored = store::load(&path);
    assert_eq!(
        stored
            .deliveries
            .iter()
            .map(|delivery| delivery.text.as_str())
            .collect::<Vec<_>>(),
        vec!["MAIL|1"],
        "the panes end with the server, so the resumed agent gets the line again"
    );
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

#[tokio::test]
async fn a_queued_delivery_keeps_its_id_across_a_restart() {
    let dir = std::env::temp_dir().join(format!(
        "herdr-orchestration-id-{}-{}",
        std::process::id(),
        unique_suffix()
    ));
    let path = dir.join(store::STORE_FILE);
    let mut f = fixture();
    f.app.orchestration = Orchestration::new(Some(path.clone()), 600);
    let delivery = f.deliver("lead", "held while focused");
    f.app.save_orchestration_now();

    f.app.orchestration = Orchestration::new(Some(path), 600);
    f.app.ensure_orchestration_loaded();
    let response = f.app.handle_agent_cancel_delivery(
        "req".into(),
        AgentCancelDeliveryParams {
            delivery_id: delivery.delivery_id.clone(),
        },
    );
    assert!(
        serde_json::from_str::<SuccessResponse>(&response).is_ok(),
        "the id a sender got before the restart still names the delivery: {response}"
    );
    assert_eq!(f.agent(0, f.supervisor).pending_deliveries, None);
    let _ = std::fs::remove_dir_all(dir);
}

fn job(name: &str, script: &str) -> JobAddParams {
    JobAddParams {
        name: name.into(),
        command: vec!["sh".into(), "-c".into(), script.into()],
        cwd: None,
        env: Default::default(),
        every_ms: Some(60_000),
        on_events: Vec::new(),
        on_path: None,
        target: Some("worker".into()),
        owner_pane_id: None,
        timeout_ms: None,
        until_ms: None,
        dedupe: None,
    }
}

impl Fixture {
    fn add_job(&mut self, params: JobAddParams) -> JobInfo {
        let response = self.app.handle_job_add("req".into(), params);
        match serde_json::from_str::<SuccessResponse>(&response)
            .unwrap_or_else(|_| panic!("job add failed: {response}"))
            .result
        {
            ResponseResult::JobInfo { job } => job,
            other => panic!("unexpected result: {other:?}"),
        }
    }

    fn job_info(&self, name: &str) -> JobInfo {
        self.app
            .job_info(self.app.orchestration.jobs.get(name).unwrap())
    }

    /// Runs ticks until the job has ended `runs` runs.
    async fn wait_for_runs(&mut self, name: &str, runs: u64) {
        for _ in 0..200 {
            self.app.drain_all_internal_events();
            if self.job_info(name).runs >= runs {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!(
            "job {name} did not reach {runs} runs: {:?}",
            self.job_info(name)
        );
    }
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn a_job_runs_at_once_and_its_output_is_delivered_to_its_target() {
    let mut f = fixture();
    let added = f.add_job(job("pr-watch", "echo \"PR 12 is red\""));
    assert_eq!(added.target_pane_id, f.app.public_pane_id(1, f.worker));
    assert_eq!(
        (added.runs, added.dedupe, added.timeout_ms),
        (0, true, 60_000)
    );

    f.tick();
    f.wait_for_runs("pr-watch", 1).await;
    assert_eq!(typed(&mut f.worker_rx).await, "PR 12 is red\r");
    let info = f.job_info("pr-watch");
    assert_eq!(info.last_exit, Some(0));
    assert_eq!(info.last_news.as_deref(), Some("PR 12 is red"));
    assert!(!info.running);
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn a_job_that_exits_non_zero_delivers_nothing() {
    let mut f = fixture();
    f.add_job(job("quiet", "echo nothing new; exit 1"));
    f.tick();
    f.wait_for_runs("quiet", 1).await;
    assert_eq!(typed(&mut f.worker_rx).await, "");
    assert_eq!(f.job_info("quiet").last_exit, Some(1));
    assert_eq!(f.job_info("quiet").last_news, None);
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn a_job_runs_on_an_event_and_gets_its_name_and_data() {
    let mut f = fixture();
    f.app.state.active = Some(1);
    f.add_job(JobAddParams {
        every_ms: None,
        on_events: vec!["pane.agent_status_changed".into()],
        target: Some("lead".into()),
        ..job(
            "on-status",
            "echo \"$HERDR_JOB_NAME $HERDR_JOB_EVENT $HERDR_JOB_TARGET_PANE_ID\"; \
             test -n \"$HERDR_JOB_EVENT_JSON\"",
        )
    });
    f.tick();
    assert_eq!(f.job_info("on-status").runs, 0);

    f.worker_state(AgentState::Working);
    f.wait_for_runs("on-status", 1).await;
    let lead_pane = f.app.public_pane_id(0, f.supervisor).unwrap();
    assert_eq!(
        typed(&mut f.supervisor_rx).await,
        format!("on-status pane.agent_status_changed {lead_pane}\r")
    );
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn a_job_runs_on_a_supervise_report_and_gets_its_fields() {
    let out = std::env::temp_dir().join(format!(
        "herdr-orchestration-report-{}-{}.json",
        std::process::id(),
        unique_suffix()
    ));
    let mut f = fixture();
    f.app.state.active = Some(1);
    f.supervise(vec![SuperviseReport::Blocked]);
    f.add_job(JobAddParams {
        every_ms: None,
        on_events: vec!["pane.supervise_report".into()],
        target: None,
        ..job(
            "on-report",
            &format!(
                "printf '%s' \"$HERDR_JOB_EVENT_JSON\" > '{}'",
                out.display()
            ),
        )
    });
    f.tick();
    f.app
        .lookup_runtime_sender(1, f.worker)
        .unwrap()
        .test_process_pty_bytes(b"Trust this folder?\r\n  1. Yes\r\n  2. No\r\n");
    f.worker_state(AgentState::Blocked);
    f.wait_for_runs("on-report", 1).await;

    let event: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap();
    let _ = std::fs::remove_file(&out);
    assert_eq!(event["event"], "pane_supervise_report");
    assert_eq!(
        event["data"],
        serde_json::json!({
            "type": "pane_supervise_report",
            "pane_id": f.app.public_pane_id(1, f.worker).unwrap(),
            "supervisor_pane_id": f.app.public_pane_id(0, f.supervisor).unwrap(),
            "agent": "worker",
            "kind": "blocked",
            "detail": "Trust this folder? / 1. Yes / 2. No",
        })
    );
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn a_job_runs_when_its_file_changes() {
    let dir = std::env::temp_dir().join(format!(
        "herdr-orchestration-path-{}-{}",
        std::process::id(),
        unique_suffix()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("proposals.json");
    std::fs::write(&file, "[]").unwrap();
    let mut f = fixture();
    f.add_job(JobAddParams {
        every_ms: None,
        on_path: Some(file.display().to_string()),
        ..job("on-file", &format!("cat {}", file.display()))
    });
    f.tick();
    f.tick();
    assert_eq!(f.job_info("on-file").runs, 0);

    std::fs::write(&file, "[\"warranted\"]").unwrap();
    f.tick();
    f.wait_for_runs("on-file", 1).await;
    assert_eq!(typed(&mut f.worker_rx).await, "[\"warranted\"]\r");
    let _ = std::fs::remove_dir_all(dir);
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn a_job_past_its_time_limit_is_stopped_and_has_no_news() {
    let mut f = fixture();
    f.add_job(JobAddParams {
        timeout_ms: Some(1_000),
        ..job("slow", "echo early; sleep 30")
    });
    f.tick();
    f.wait_for_runs("slow", 1).await;
    assert_eq!(f.job_info("slow").last_exit, None);
    assert_eq!(typed(&mut f.worker_rx).await, "");
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn a_job_ends_when_its_command_exits_and_a_process_it_left_keeps_its_output_open() {
    let mut f = fixture();
    f.add_job(job("leaver", "sleep 30 & echo up"));
    f.tick();
    f.wait_for_runs("leaver", 1).await;
    assert_eq!(f.job_info("leaver").last_exit, Some(0));
    assert_eq!(typed(&mut f.worker_rx).await, "up\r");
}

#[cfg(unix)]
fn process_runs(pid: &str) -> bool {
    std::process::Command::new("kill")
        .args(["-0", pid])
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn removing_a_job_stops_its_running_command_and_what_that_command_started() {
    let dir = std::env::temp_dir().join(format!(
        "herdr-orchestration-kill-{}-{}",
        std::process::id(),
        unique_suffix()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let pids = dir.join("pids");
    let mut f = fixture();
    f.add_job(job(
        "long",
        &format!("sleep 60 & echo $! $$ > {}; wait", pids.display()),
    ));
    f.tick();
    let mut started = String::new();
    for _ in 0..100 {
        started = std::fs::read_to_string(&pids).unwrap_or_default();
        if started.split_whitespace().count() == 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let started: Vec<&str> = started.split_whitespace().collect();
    assert_eq!(started.len(), 2, "the command did not start");
    assert!(started.iter().all(|pid| process_runs(pid)));

    let response = f.app.handle_job_remove(
        "req".into(),
        JobTarget {
            name: "long".into(),
        },
    );
    assert!(serde_json::from_str::<SuccessResponse>(&response).is_ok());
    for _ in 0..100 {
        if !started.iter().any(|pid| process_runs(pid)) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        !started.iter().any(|pid| process_runs(pid)),
        "the command or its child still runs"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn a_job_whose_folder_is_gone_does_not_run_somewhere_else() {
    let mut f = fixture();
    f.add_job(JobAddParams {
        cwd: Some("/nonexistent/herdr-orchestration".into()),
        ..job("lost", "echo ran in $PWD")
    });
    f.tick();
    f.wait_for_runs("lost", 1).await;
    assert_eq!(f.job_info("lost").last_exit, None);
    assert_eq!(typed(&mut f.worker_rx).await, "");
}

#[tokio::test]
async fn a_job_is_removed_when_its_owner_or_target_pane_closes() {
    let mut f = fixture();
    let owner = f.app.public_pane_id(1, f.worker).unwrap();
    f.add_job(JobAddParams {
        every_ms: None,
        on_events: vec!["pane.closed".into()],
        target: Some("lead".into()),
        owner_pane_id: Some(owner),
        ..job("owned", "true")
    });
    f.app.state.workspaces.remove(1);
    f.tick();
    assert_eq!(f.app.orchestration.jobs.len(), 0);
}

#[tokio::test]
async fn a_job_with_the_same_name_replaces_the_first_and_a_bad_request_is_refused() {
    let mut f = fixture();
    let event_job = |script: &str| JobAddParams {
        every_ms: None,
        on_events: vec!["pane.closed".into()],
        ..job("watch", script)
    };
    f.add_job(event_job("echo one"));
    let second = f.add_job(event_job("echo two"));
    assert_eq!(second.command[2], "echo two");
    assert_eq!(f.app.orchestration.jobs.len(), 1);

    let missing_target = f.app.handle_job_add(
        "req".into(),
        JobAddParams {
            target: Some("nobody".into()),
            ..event_job("true")
        },
    );
    assert_eq!(error_code(&missing_target), "pane_not_found");
    let no_trigger = f.app.handle_job_add(
        "req".into(),
        JobAddParams {
            on_events: Vec::new(),
            ..event_job("true")
        },
    );
    assert_eq!(error_code(&no_trigger), "invalid_job_trigger");

    let remove = |f: &mut Fixture| {
        f.app.handle_job_remove(
            "req".into(),
            JobTarget {
                name: "watch".into(),
            },
        )
    };
    assert!(serde_json::from_str::<SuccessResponse>(&remove(&mut f)).is_ok());
    assert_eq!(error_code(&remove(&mut f)), "job_not_found");
}

#[tokio::test]
async fn jobs_come_back_from_the_state_file_with_their_last_news() {
    let dir = std::env::temp_dir().join(format!(
        "herdr-orchestration-jobs-{}-{}",
        std::process::id(),
        unique_suffix()
    ));
    let path = dir.join(store::STORE_FILE);
    let mut f = fixture();
    f.app.orchestration = Orchestration::new(Some(path.clone()), 600);
    f.add_job(JobAddParams {
        every_ms: None,
        on_events: vec!["pane.closed".into()],
        ..job("kept", "echo red")
    });
    let run = f
        .app
        .orchestration
        .jobs
        .begin("kept", None, Instant::now())
        .unwrap();
    f.app
        .handle_job_finished("kept", run.run_id, Some(0), "red\n");
    f.app.save_orchestration_now();

    f.app.orchestration = Orchestration::new(Some(path), 600);
    f.app.ensure_orchestration_loaded();
    let info = f.job_info("kept");
    assert_eq!(info.on_events, vec!["pane.closed".to_string()]);
    assert_eq!(info.target_pane_id, f.app.public_pane_id(1, f.worker));
    assert_eq!(info.last_news.as_deref(), Some("red"));
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn pane_tokens_without_a_time_limit_come_back_from_the_state_file() {
    let dir = std::env::temp_dir().join(format!(
        "herdr-orchestration-tokens-{}-{}",
        std::process::id(),
        unique_suffix()
    ));
    let path = dir.join(store::STORE_FILE);
    let mut f = fixture();
    f.app.orchestration = Orchestration::new(Some(path.clone()), 600);
    let worker_pane = f.app.public_pane_id(1, f.worker).unwrap();
    let report = |f: &mut Fixture, key: &str, value: &str, ttl_ms: Option<u64>| {
        let request = serde_json::json!({
            "id": "req",
            "method": "pane.report_metadata",
            "params": {
                "pane_id": worker_pane,
                "source": "tree",
                "tokens": { key: value },
                "ttl_ms": ttl_ms,
            },
        });
        let response = f
            .app
            .handle_api_request(serde_json::from_value(request).unwrap());
        assert!(
            serde_json::from_str::<SuccessResponse>(&response).is_ok(),
            "{response}"
        );
    };
    report(&mut f, "label", "SRE-194", None);
    report(&mut f, "progress", "3/5", Some(60_000));
    f.app.save_orchestration_now();

    let terminal = f.terminal(1, f.worker);
    f.app
        .state
        .terminals
        .get_mut(&terminal)
        .unwrap()
        .metadata_tokens = Default::default();
    f.app.orchestration = Orchestration::new(Some(path), 600);
    f.app.ensure_orchestration_loaded();
    let tokens = f.agent(1, f.worker).tokens;
    assert_eq!(tokens.get("label").map(String::as_str), Some("SRE-194"));
    assert_eq!(tokens.get("progress"), None);
    let _ = std::fs::remove_dir_all(dir);
}

impl Fixture {
    fn show_on_worker(&mut self, text: &str) {
        self.app
            .lookup_runtime_sender(1, self.worker)
            .unwrap()
            .test_process_pty_bytes(text.as_bytes());
    }

    fn answer(&mut self, key: &str, dialog: &str) -> String {
        let request = serde_json::json!({
            "id": "req",
            "method": "agent.answer",
            "params": { "target": "worker", "key": key, "dialog": dialog },
        });
        self.app
            .handle_api_request(serde_json::from_value(request).unwrap())
    }
}

const DIALOG_A: &str = "Run rm -rf build?\r\n\u{276f} 1. Yes\r\n  2. No\r\n";

#[tokio::test]
async fn an_answer_presses_its_key_when_the_agent_still_shows_the_same_dialog() {
    let mut f = fixture();
    f.show_on_worker(DIALOG_A);
    f.worker_state(AgentState::Blocked);
    let agent = f.agent(1, f.worker);
    assert_eq!(
        agent.blocker_lines,
        vec!["Run rm -rf build?", "\u{276f} 1. Yes", "2. No"]
    );
    let dialog = agent.dialog.expect("a blocked agent names its dialog");

    let response = f.answer("2", &dialog);
    assert!(
        serde_json::from_str::<SuccessResponse>(&response).is_ok(),
        "{response}"
    );
    assert_eq!(typed_keys(&mut f.worker_rx), "2");
}

#[tokio::test]
async fn an_answer_is_refused_when_the_agent_shows_another_dialog_now() {
    let mut f = fixture();
    f.show_on_worker(DIALOG_A);
    f.worker_state(AgentState::Blocked);
    let old = f.agent(1, f.worker).dialog.unwrap();

    // The agent moved to a second dialog with no state change in between.
    f.show_on_worker(
        "\x1b[2J\x1b[HAlways allow rm?\r\n\u{276f} 1. Yes, and do not ask again\r\n  2. No\r\n",
    );
    assert_eq!(error_code(&f.answer("1", &old)), "dialog_changed");
    assert_eq!(typed_keys(&mut f.worker_rx), "");

    // The record follows the screen, so a menu made after the change can answer.
    f.tick();
    let agent = f.agent(1, f.worker);
    assert_eq!(agent.blocker_lines[0], "Always allow rm?");
    let new = agent.dialog.unwrap();
    assert_ne!(new, old);
    assert!(serde_json::from_str::<SuccessResponse>(&f.answer("2", &new)).is_ok());
    assert_eq!(typed_keys(&mut f.worker_rx), "2");
}

#[tokio::test]
async fn an_answer_is_refused_when_the_agent_is_not_blocked_or_the_key_is_not_one_digit() {
    let mut f = fixture();
    f.show_on_worker(DIALOG_A);
    f.worker_state(AgentState::Blocked);
    let dialog = f.agent(1, f.worker).dialog.unwrap();
    for key in ["0", "12", "y", "enter", "ctrl+c", "\u{3}", ""] {
        assert_eq!(
            error_code(&f.answer(key, &dialog)),
            "invalid_answer_key",
            "{key:?}"
        );
    }
    f.worker_state(AgentState::Idle);
    assert_eq!(f.agent(1, f.worker).dialog, None);
    assert_eq!(error_code(&f.answer("1", &dialog)), "agent_not_blocked");
    assert_eq!(typed_keys(&mut f.worker_rx), "");
}

/// The keys written to a pane so far. A key has no Enter after it, so nothing is waited on.
fn typed_keys(rx: &mut tokio::sync::mpsc::Receiver<Bytes>) -> String {
    let mut bytes = Vec::new();
    while let Ok(chunk) = rx.try_recv() {
        bytes.extend_from_slice(&chunk);
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

impl Fixture {
    fn keys(&mut self, ws_idx: usize, pane: PaneId, keys: &[deliveries::DraftKey]) {
        let terminal = self.terminal(ws_idx, pane);
        self.app.note_pane_keys(&terminal, keys);
    }

    fn pending(&self, ws_idx: usize, pane: PaneId) -> u32 {
        self.app
            .orchestration
            .pending_for(&self.terminal(ws_idx, pane))
    }
}

#[tokio::test]
async fn a_delivery_is_typed_when_the_text_a_person_typed_is_erased() {
    use deliveries::DraftKey;
    let mut f = fixture();
    f.keys(1, f.worker, &[DraftKey::Text(1)]);
    let held = f.deliver("worker", "wake");
    assert_eq!(held.state, DeliveryState::Queued);
    assert_eq!(held.held_by.as_deref(), Some("unsent_input"));

    f.keys(1, f.worker, &[DraftKey::Erase]);
    f.app.run_orchestration_tick(Instant::now());
    assert_eq!(typed(&mut f.worker_rx).await, "wake\r");
}

#[tokio::test]
async fn text_typed_at_a_dialog_is_an_answer_and_does_not_hold_deliveries() {
    use deliveries::DraftKey;
    let mut f = fixture();
    f.worker_state(AgentState::Blocked);
    f.keys(1, f.worker, &[DraftKey::Text(1)]);
    f.worker_state(AgentState::Idle);
    assert_eq!(f.deliver("worker", "wake").state, DeliveryState::Delivered);
}

#[tokio::test]
async fn an_agent_that_starts_to_work_took_the_text_and_text_typed_while_it_works_stays() {
    use deliveries::DraftKey;
    let mut f = fixture();
    f.keys(1, f.worker, &[DraftKey::Text(4)]);
    f.worker_state(AgentState::Working);
    f.worker_state(AgentState::Idle);
    assert_eq!(f.deliver("worker", "first").state, DeliveryState::Delivered);
    assert_eq!(typed(&mut f.worker_rx).await, "first\r");

    // A follow-up typed while the agent works is still in the input when the work ends.
    f.worker_state(AgentState::Working);
    f.keys(1, f.worker, &[DraftKey::Text(4)]);
    let terminal = f.terminal(1, f.worker);
    f.app
        .note_agent_state_change(1, f.worker, AgentState::Working, AgentState::Working);
    f.worker_state(AgentState::Idle);
    assert_eq!(f.deliver("worker", "second").state, DeliveryState::Queued);
    assert!(f.app.orchestration.drafts.holds(&terminal, Instant::now()));
}

#[tokio::test]
async fn a_delivery_waits_while_a_client_is_attached_to_the_terminal_alone() {
    let mut f = fixture();
    let terminal = f.terminal(1, f.worker);
    f.app.orchestration.attached_terminals.insert(terminal);
    let held = f.deliver("worker", "wake");
    assert_eq!(held.held_by.as_deref(), Some("focused"));
    f.app.orchestration.attached_terminals.clear();
    f.tick();
    assert_eq!(typed(&mut f.worker_rx).await, "wake\r");
}

#[tokio::test]
async fn a_repeat_of_the_blocked_state_is_not_a_second_report() {
    let mut f = fixture();
    f.app.state.active = Some(1);
    f.supervise(vec![SuperviseReport::Blocked]);
    f.worker_state(AgentState::Blocked);
    assert!(typed(&mut f.supervisor_rx)
        .await
        .starts_with("HERDR|blocked|"));
    f.app
        .note_agent_state_change(1, f.worker, AgentState::Blocked, AgentState::Blocked);
    f.tick();
    assert_eq!(typed(&mut f.supervisor_rx).await, "");
}

#[tokio::test]
async fn a_newer_report_takes_the_place_of_one_that_still_waits() {
    let mut f = fixture();
    f.app.state.active = Some(1);
    f.supervise(vec![SuperviseReport::Blocked, SuperviseReport::Exited]);
    // The supervisor works, so its reports wait.
    f.set_agent(0, f.supervisor, "lead", AgentState::Working);
    for _ in 0..3 {
        f.worker_state(AgentState::Blocked);
        f.worker_state(AgentState::Working);
    }
    assert_eq!(f.pending(0, f.supervisor), 1);
    // Mail from another sender stays.
    f.deliver("lead", "HERDR|blocked|worker|w2:p1|not from the server");
    assert_eq!(f.pending(0, f.supervisor), 2);

    let worker_pane = f.app.public_pane_id(1, f.worker).unwrap();
    f.app.state.workspaces.remove(1);
    f.tick();
    assert_eq!(f.pending(0, f.supervisor), 2);
    f.set_agent(0, f.supervisor, "lead", AgentState::Idle);
    f.tick();
    assert_eq!(
        typed(&mut f.supervisor_rx).await,
        "HERDR|blocked|worker|w2:p1|not from the server\r"
    );
    f.tick();
    assert_eq!(
        typed(&mut f.supervisor_rx).await,
        format!("HERDR|exited|worker|{worker_pane}\r")
    );
}

#[tokio::test]
async fn the_same_screen_at_a_later_wait_is_another_dialog() {
    let mut f = fixture();
    f.show_on_worker(DIALOG_A);
    f.worker_state(AgentState::Blocked);
    let first = f.agent(1, f.worker).dialog.unwrap();

    // The dialog was answered in the pane, and the agent asks the same thing again.
    f.worker_state(AgentState::Working);
    f.worker_state(AgentState::Blocked);
    let second = f.agent(1, f.worker).dialog.unwrap();
    assert_ne!(first, second);
    assert_eq!(error_code(&f.answer("1", &first)), "dialog_changed");
    assert_eq!(typed_keys(&mut f.worker_rx), "");
    assert!(serde_json::from_str::<SuccessResponse>(&f.answer("1", &second)).is_ok());
}

#[tokio::test]
async fn a_saved_delivery_gets_the_checks_of_a_request_when_it_loads() {
    let dir = std::env::temp_dir().join(format!(
        "herdr-orchestration-load-{}-{}",
        std::process::id(),
        unique_suffix()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(store::STORE_FILE);
    let mut f = fixture();
    let pane = f.app.public_pane_id(1, f.worker).unwrap();
    let saved = serde_json::json!({
        "deliveries": [
            { "pane_id": pane, "text": "go\u{1b}[201~\rnow", "queued_at_ms": 1 },
            { "pane_id": pane, "text": "\u{1b}\r", "queued_at_ms": 2 },
        ],
        "jobs": [{
            "name": "loop",
            "command": ["true"],
            "on_events": ["pane.delivery_changed"],
            "target_pane_id": pane,
        }],
    });
    std::fs::write(&path, saved.to_string()).unwrap();
    f.worker_state(AgentState::Working);
    f.app.orchestration = Orchestration::new(Some(path), 600);
    f.app.ensure_orchestration_loaded();
    assert_eq!(f.pending(1, f.worker), 1);
    assert!(f.app.orchestration.jobs.get("loop").is_none());
    f.worker_state(AgentState::Idle);
    f.tick();
    assert_eq!(typed(&mut f.worker_rx).await, "go[201~now\r");
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn a_save_inside_the_save_interval_does_not_make_the_tick_run_on_every_pass() {
    let dir = std::env::temp_dir().join(format!(
        "herdr-orchestration-save-{}-{}",
        std::process::id(),
        unique_suffix()
    ));
    let mut f = fixture();
    f.app.orchestration = Orchestration::new(Some(dir.join(store::STORE_FILE)), 600);
    f.worker_state(AgentState::Working);
    f.deliver("worker", "one");
    f.app.run_orchestration_tick(Instant::now());
    // A second change inside the interval is not saved yet, and the tick is not due again.
    f.deliver("worker", "two");
    let now = Instant::now();
    f.app.run_orchestration_tick(now);
    assert!(f.app.orchestration.dirty);
    assert!(!f.app.orchestration.tick_due(now));
    assert!(f.app.orchestration.tick_due(now + SAVE_INTERVAL));
    let _ = std::fs::remove_dir_all(dir);
}

#[cfg(unix)]
impl Fixture {
    /// Runs ticks, which start held triggers, until the job has ended `runs` runs.
    async fn tick_until_runs(&mut self, name: &str, runs: u64) {
        for _ in 0..300 {
            self.tick();
            self.app.drain_all_internal_events();
            if self.job_info(name).runs >= runs {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!(
            "job {name} did not reach {runs} runs: {:?}",
            self.job_info(name)
        );
    }
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn an_event_that_comes_while_its_job_runs_starts_one_more_run() {
    let mut f = fixture();
    f.add_job(JobAddParams {
        every_ms: None,
        on_events: vec!["pane.agent_status_changed".into()],
        target: None,
        ..job("twice", "true")
    });
    f.worker_state(AgentState::Working);
    f.worker_state(AgentState::Idle);
    f.tick_until_runs("twice", 2).await;
    tokio::time::sleep(Duration::from_millis(1200)).await;
    f.tick();
    f.app.drain_all_internal_events();
    assert_eq!(f.job_info("twice").runs, 2);
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn news_that_a_full_queue_refused_is_delivered_by_a_later_run() {
    let mut f = fixture();
    f.worker_state(AgentState::Working);
    for _ in 0..deliveries::MAX_PENDING_PER_TERMINAL {
        f.deliver("worker", "held");
    }
    f.add_job(JobAddParams {
        every_ms: None,
        on_events: vec!["pane.closed".into()],
        ..job("ci", "echo red")
    });
    let run = |f: &mut Fixture| {
        f.app
            .handle_job_run("req".into(), JobTarget { name: "ci".into() })
    };
    run(&mut f);
    f.tick_until_runs("ci", 1).await;
    assert_eq!(f.job_info("ci").last_news, None);

    let worker = f.terminal(1, f.worker);
    f.app.orchestration.deliveries.drop_terminal(&worker);
    run(&mut f);
    f.tick_until_runs("ci", 2).await;
    assert_eq!(f.job_info("ci").last_news.as_deref(), Some("red"));
    assert_eq!(f.pending(1, f.worker), 1);
}

#[cfg(unix)]
impl Fixture {
    /// Starts a job whose command starts a child and waits. Gives the two process ids.
    async fn start_job_with_child(
        &mut self,
        params: JobAddParams,
        pids: &std::path::Path,
    ) -> Vec<String> {
        self.add_job(params);
        self.tick();
        let mut started = String::new();
        for _ in 0..100 {
            started = std::fs::read_to_string(pids).unwrap_or_default();
            if started.split_whitespace().count() == 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let started: Vec<String> = started.split_whitespace().map(str::to_string).collect();
        assert_eq!(started.len(), 2, "the command did not start");
        assert!(started.iter().all(|pid| process_runs(pid)));
        started
    }
}

#[cfg(unix)]
async fn all_gone(pids: &[String]) -> bool {
    for _ in 0..150 {
        if !pids.iter().any(|pid| process_runs(pid)) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    false
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn a_job_past_its_time_limit_stops_what_its_command_started_too() {
    let dir = std::env::temp_dir().join(format!(
        "herdr-orchestration-limit-{}-{}",
        std::process::id(),
        unique_suffix()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let pids = dir.join("pids");
    let mut f = fixture();
    let started = f
        .start_job_with_child(
            JobAddParams {
                timeout_ms: Some(1_000),
                ..job(
                    "limit",
                    &format!("sleep 60 & echo $! $$ > {}; wait", pids.display()),
                )
            },
            &pids,
        )
        .await;
    f.wait_for_runs("limit", 1).await;
    assert!(
        all_gone(&started).await,
        "the command or its child still runs"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn the_end_of_the_server_stops_the_commands_of_running_jobs() {
    let dir = std::env::temp_dir().join(format!(
        "herdr-orchestration-close-{}-{}",
        std::process::id(),
        unique_suffix()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let pids = dir.join("pids");
    let mut f = fixture();
    let started = f
        .start_job_with_child(
            job(
                "long",
                &format!("sleep 60 & echo $! $$ > {}; wait", pids.display()),
            ),
            &pids,
        )
        .await;
    f.app.close_orchestration();
    assert!(
        all_gone(&started).await,
        "the command or its child still runs"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn output_printed_before_a_process_that_left_the_group_is_still_news() {
    if std::process::Command::new("setsid")
        .arg("true")
        .status()
        .is_err()
    {
        return;
    }
    let mut f = fixture();
    f.add_job(job("detached", "echo up; setsid sleep 5 & exit 0"));
    f.tick();
    f.wait_for_runs("detached", 1).await;
    assert_eq!(typed(&mut f.worker_rx).await, "up\r");
}

impl Fixture {
    /// Makes the last thing a person did in a pane `secs` seconds old.
    fn quiet_for(&mut self, ws_idx: usize, pane: PaneId, secs: u64) {
        let terminal = self.terminal(ws_idx, pane);
        let at = Instant::now()
            .checked_sub(Duration::from_secs(secs))
            .unwrap();
        self.app.orchestration.activity.insert(terminal, at);
    }
}

#[tokio::test]
async fn a_focused_pane_takes_a_delivery_once_a_person_is_quiet_in_it() {
    let mut f = fixture();
    assert_eq!(
        f.deliver("lead", "wake").held_by.as_deref(),
        Some("focused")
    );
    f.quiet_for(0, f.supervisor, 29);
    f.tick();
    assert_eq!(typed(&mut f.supervisor_rx).await, "");

    f.quiet_for(0, f.supervisor, 31);
    f.tick();
    assert_eq!(typed(&mut f.supervisor_rx).await, "wake\r");
}

#[tokio::test]
async fn a_key_in_the_focused_pane_starts_its_quiet_time_again() {
    let mut f = fixture();
    f.deliver("lead", "wake");
    f.quiet_for(0, f.supervisor, 31);
    let terminal = f.terminal(0, f.supervisor);
    // An arrow key leaves no text in the input and still counts.
    f.app.note_pane_keys(&terminal, &[]);
    f.tick();
    assert_eq!(typed(&mut f.supervisor_rx).await, "");
}

#[tokio::test]
async fn focus_that_comes_to_a_pane_starts_its_quiet_time() {
    let mut f = fixture();
    f.tick();
    f.quiet_for(0, f.supervisor, 31);
    f.app.state.active = Some(1);
    f.tick();
    f.app.state.active = Some(0);
    assert_eq!(
        f.deliver("lead", "wake").held_by.as_deref(),
        Some("focused")
    );
}

#[tokio::test]
async fn a_focused_pane_that_is_scrolled_back_holds_deliveries_past_the_quiet_time() {
    let mut f = fixture();
    let lines: String = (0..200).map(|n| format!("line {n}\r\n")).collect();
    let (runtime, rx) = crate::terminal::TerminalRuntime::test_with_channel_and_scrollback_bytes(
        80,
        24,
        1 << 20,
        lines.as_bytes(),
        64,
    );
    runtime.set_scroll_offset_from_bottom(5);
    f.app.state.insert_test_runtime(f.supervisor, runtime);
    f.supervisor_rx = rx;

    f.deliver("lead", "wake");
    f.quiet_for(0, f.supervisor, 31);
    f.tick();
    assert_eq!(typed(&mut f.supervisor_rx).await, "");
    let terminal = f.terminal(0, f.supervisor);
    assert_eq!(
        f.app
            .pane_readiness(&terminal)
            .and_then(PaneReadiness::hold),
        Some("scrolled_back")
    );

    f.quiet_for(0, f.supervisor, deliveries::DRAFT_HOLD.as_secs() + 1);
    f.tick();
    assert_eq!(typed(&mut f.supervisor_rx).await, "wake\r");
}

#[tokio::test]
async fn no_focus_hold_limit_holds_a_delivery_while_the_pane_is_focused() {
    let mut f = fixture();
    f.app.orchestration.focus_hold = None;
    f.deliver("lead", "wake");
    f.quiet_for(0, f.supervisor, 3600);
    f.tick();
    assert_eq!(typed(&mut f.supervisor_rx).await, "");
}

impl Fixture {
    /// `opened_for_worktrees` as `workspace.get` and `workspace.list` report it.
    fn opened_for_worktrees(&mut self, ws_idx: usize) -> (bool, bool) {
        let workspace_id = self.app.public_workspace_id(ws_idx);
        let get = self.app.handle_api_request(
            serde_json::from_value(serde_json::json!({
                "id": "req",
                "method": "workspace.get",
                "params": { "workspace_id": workspace_id },
            }))
            .unwrap(),
        );
        let get: serde_json::Value = serde_json::from_str(&get).unwrap();
        let list = self.app.handle_api_request(
            serde_json::from_value(serde_json::json!({
                "id": "req",
                "method": "workspace.list",
                "params": {},
            }))
            .unwrap(),
        );
        let list: serde_json::Value = serde_json::from_str(&list).unwrap();
        let listed = list["result"]["workspaces"]
            .as_array()
            .unwrap()
            .iter()
            .find(|workspace| workspace["workspace_id"] == workspace_id.as_str())
            .unwrap()["opened_for_worktrees"]
            .clone();
        (
            get["result"]["workspace"]["opened_for_worktrees"] == true,
            listed == true,
        )
    }

    fn workspace_updates(&self) -> Vec<(String, bool)> {
        self.app
            .event_hub
            .events_after(0)
            .into_iter()
            .filter_map(|(_, event)| match event.data {
                EventData::WorkspaceUpdated { workspace } => {
                    Some((workspace.workspace_id, workspace.opened_for_worktrees))
                }
                _ => None,
            })
            .collect()
    }
}

#[tokio::test]
async fn a_workspace_opened_for_worktrees_keeps_its_mark_until_it_is_renamed() {
    let mut f = fixture();
    assert_eq!(f.opened_for_worktrees(1), (false, false));
    f.app.mark_opened_for_worktrees(1);
    assert_eq!(f.opened_for_worktrees(1), (true, true));
    assert_eq!(f.opened_for_worktrees(0), (false, false));

    // The one pane Herdr opened does not clear it.
    let pane = f.app.pane_info(1, f.worker).unwrap();
    f.app.emit_event(EventEnvelope {
        event: EventKind::PaneCreated,
        data: EventData::PaneCreated { pane },
    });
    assert_eq!(f.opened_for_worktrees(1), (true, true));

    let workspace_id = f.app.public_workspace_id(1);
    let response = f.app.handle_api_request(
        serde_json::from_value(serde_json::json!({
            "id": "req",
            "method": "workspace.rename",
            "params": { "workspace_id": workspace_id, "label": "mine" },
        }))
        .unwrap(),
    );
    assert!(
        serde_json::from_str::<SuccessResponse>(&response).is_ok(),
        "{response}"
    );
    assert_eq!(f.opened_for_worktrees(1), (false, false));
    assert_eq!(f.workspace_updates(), vec![(workspace_id, false)]);
}

#[tokio::test]
async fn a_workspace_opened_for_worktrees_loses_its_mark_with_a_second_pane() {
    let mut f = fixture();
    f.app.mark_opened_for_worktrees(1);
    let second = f.app.state.workspaces[1].test_split(ratatui::layout::Direction::Horizontal);
    f.app.state.ensure_test_terminals();
    let pane = f.app.pane_info(1, second).unwrap();
    f.app.emit_event(EventEnvelope {
        event: EventKind::PaneCreated,
        data: EventData::PaneCreated { pane },
    });
    assert_eq!(f.opened_for_worktrees(1), (false, false));
    let events: Vec<EventKind> = f
        .app
        .event_hub
        .events_after(0)
        .into_iter()
        .map(|(_, event)| event.event)
        .collect();
    assert_eq!(
        events,
        vec![EventKind::PaneCreated, EventKind::WorkspaceUpdated]
    );
}

#[tokio::test]
async fn a_closed_workspace_drops_its_mark() {
    let mut f = fixture();
    f.app.mark_opened_for_worktrees(1);
    let workspace_id = f.app.public_workspace_id(1);
    f.app.emit_event(EventEnvelope {
        event: EventKind::WorkspaceClosed,
        data: EventData::WorkspaceClosed {
            workspace_id,
            workspace: None,
        },
    });
    assert!(f.app.orchestration.opened_for_worktrees.is_empty());
    assert!(f.workspace_updates().is_empty());
}

#[tokio::test]
async fn a_workspace_opened_for_worktrees_keeps_its_mark_across_the_state_file() {
    let dir = std::env::temp_dir().join(format!(
        "herdr-orchestration-worktree-parents-{}-{}",
        std::process::id(),
        unique_suffix()
    ));
    let path = dir.join(store::STORE_FILE);
    let mut f = fixture();
    f.app.orchestration = Orchestration::new(Some(path.clone()), 600);
    f.app.mark_opened_for_worktrees(1);
    f.app
        .orchestration
        .opened_for_worktrees
        .insert("gone".into());
    f.app.save_orchestration_now();
    let saved: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(
        saved["opened_for_worktrees"],
        serde_json::json!([f.app.public_workspace_id(1)])
    );

    f.app.orchestration = Orchestration::new(Some(path), 600);
    f.app.ensure_orchestration_loaded();
    assert_eq!(f.opened_for_worktrees(1), (true, true));
    assert_eq!(f.opened_for_worktrees(0), (false, false));
    assert_eq!(f.app.orchestration.opened_for_worktrees.len(), 1);
    let _ = std::fs::remove_dir_all(dir);
}

#[path = "pause_tests.rs"]
mod pause_tests;
