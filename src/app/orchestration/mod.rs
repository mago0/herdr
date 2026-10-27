//! Server-side support for agents that supervise other agents: prompts delivered when a pane is
//! safe to type into, supervision links with reports, and detection of stalled agents.

pub(crate) mod deliveries;
pub(crate) mod jobs;
pub(crate) mod stall;
mod store;
pub(crate) mod supervision;
#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::api::schema::{
    AgentCancelDeliveryParams, AgentDeliverParams, AgentDeliveriesParams, DeliveryInfo,
    DeliveryState, EventData, EventEnvelope, EventKind, JobAddParams, JobInfo, JobTarget,
    PaneSuperviseParams, ResponseResult, SuperviseReport,
};
use crate::app::api::responses::{encode_error, encode_success};
use crate::app::terminal_targets::TerminalTargetError;
use crate::app::App;
use crate::detect::AgentState;
use crate::terminal::TerminalId;

use deliveries::{Delivery, DeliveryQueue, PaneReadiness};
use jobs::{JobRun, JobSpec, JobTable, TriggerEvent};
use stall::{StallChange, StallTable};
use supervision::Supervision;

const TICK: Duration = Duration::from_secs(1);
const SAVE_INTERVAL: Duration = Duration::from_secs(1);
const MAX_DELIVERY_BYTES: usize = 16 * 1024;
const SUPERVISE_SOURCE: &str = "supervise";

/// The parts of an agent record that come from this module.
pub(crate) struct AgentOrchestrationFields {
    pub(crate) supervisor_pane_id: Option<String>,
    pub(crate) pending_deliveries: Option<u32>,
    pub(crate) stalled_secs: Option<u64>,
    pub(crate) blocker: Option<String>,
    pub(crate) blocker_lines: Vec<String>,
    pub(crate) dialog: Option<String>,
}

/// Why a delivery was not queued.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DeliveryRefusal {
    /// Nothing is left of the text once the characters a terminal reads as keys are removed.
    Empty,
    QueueFull,
}

struct InFlight {
    delivery: Delivery,
    completion: std::sync::mpsc::Receiver<std::io::Result<()>>,
}

/// What a supervised pane last looked like, kept for the report sent after it is gone.
#[derive(Debug, Clone)]
struct LastSeen {
    name: String,
    pane_id: String,
    had_agent: bool,
}

pub(crate) struct Orchestration {
    pub(crate) deliveries: DeliveryQueue,
    in_flight: HashMap<TerminalId, InFlight>,
    pub(crate) links: HashMap<TerminalId, Supervision>,
    last_seen: HashMap<TerminalId, LastSeen>,
    pub(crate) blockers: HashMap<TerminalId, supervision::Blocker>,
    /// Terminals whose input holds text a person typed and has not sent.
    drafts: deliveries::DraftTracker,
    /// Terminals that a client is attached to alone, outside the shell.
    pub(crate) attached_terminals: std::collections::HashSet<TerminalId>,
    /// When a person last pressed a key in a terminal or brought it into view.
    pub(crate) activity: HashMap<TerminalId, Instant>,
    /// The terminals in view at the last look, with how far each is scrolled back.
    in_view: HashMap<TerminalId, usize>,
    /// How long a focused pane holds deliveries after the last activity in it. None holds them
    /// as long as the pane is focused.
    pub(crate) focus_hold: Option<Duration>,
    /// Counts each time an agent starts to wait at a dialog, so that two dialogs have two names.
    block_count: u64,
    pub(crate) stalls: StallTable,
    pub(crate) stall_after: Option<Duration>,
    pub(crate) jobs: JobTable,
    next_tick: Option<Instant>,
    /// A person can type into a pane only while a client is attached.
    pub(crate) client_attached: bool,
    store_path: Option<PathBuf>,
    loaded: bool,
    dirty: bool,
    last_save: Option<Instant>,
}

impl Orchestration {
    pub(crate) fn new(store_path: Option<PathBuf>, stall_after_secs: u32) -> Self {
        Self {
            deliveries: DeliveryQueue::default(),
            in_flight: HashMap::new(),
            links: HashMap::new(),
            last_seen: HashMap::new(),
            blockers: HashMap::new(),
            drafts: deliveries::DraftTracker::default(),
            attached_terminals: std::collections::HashSet::new(),
            activity: HashMap::new(),
            in_view: HashMap::new(),
            focus_hold: focus_hold(DEFAULT_FOCUS_HOLD_SECS),
            block_count: 0,
            stalls: StallTable::default(),
            stall_after: stall_after(stall_after_secs),
            jobs: JobTable::default(),
            next_tick: None,
            client_attached: true,
            store_path,
            loaded: false,
            dirty: false,
            last_save: None,
        }
    }

    pub(crate) fn production_store_path() -> PathBuf {
        crate::session::data_dir().join(store::STORE_FILE)
    }

    pub(crate) fn tick_due(&self, now: Instant) -> bool {
        self.next_tick.is_none_or(|at| now >= at)
    }

    /// Makes the next loop pass run the tick, for a change that can make a pane safe.
    pub(crate) fn wake(&mut self) {
        self.next_tick = None;
    }

    /// Pane tokens are part of the saved state, so a change to them is saved on the next tick.
    pub(crate) fn note_tokens_changed(&mut self) {
        self.dirty = true;
    }

    fn pending_for(&self, terminal: &TerminalId) -> u32 {
        self.deliveries.pending_for(terminal) + u32::from(self.in_flight.contains_key(terminal))
    }
}

pub(crate) const DEFAULT_FOCUS_HOLD_SECS: u32 = 30;

/// The focus hold for a configured number of seconds, where 0 means no limit.
pub(crate) fn focus_hold(secs: u32) -> Option<Duration> {
    (secs > 0).then(|| Duration::from_secs(secs.into()))
}

pub(crate) fn stall_after(secs: u32) -> Option<Duration> {
    (secs > 0).then(|| Duration::from_secs(secs.into()))
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

fn target_error(id: String, err: TerminalTargetError) -> String {
    match err {
        TerminalTargetError::NotFound { target } => {
            encode_error(id, "pane_not_found", format!("target {target} not found"))
        }
        TerminalTargetError::Ambiguous { target, .. } => encode_error(
            id,
            "ambiguous_target",
            format!("target {target} matches more than one pane"),
        ),
    }
}

impl App {
    pub(crate) fn terminal_location(
        &self,
        terminal: &TerminalId,
    ) -> Option<(usize, crate::layout::PaneId)> {
        self.state
            .workspaces
            .iter()
            .enumerate()
            .find_map(|(ws_idx, ws)| {
                ws.tabs.iter().find_map(|tab| {
                    tab.panes.iter().find_map(|(pane_id, pane)| {
                        (&pane.attached_terminal_id == terminal).then_some((ws_idx, *pane_id))
                    })
                })
            })
    }

    pub(crate) fn public_pane_id_of_terminal(&self, terminal: &TerminalId) -> Option<String> {
        let (ws_idx, pane_id) = self.terminal_location(terminal)?;
        self.public_pane_id(ws_idx, pane_id)
    }

    /// Resolves a terminal id, a pane id or an agent name to the terminal behind it.
    pub(crate) fn terminal_of_target(
        &self,
        target: &str,
    ) -> Result<TerminalId, TerminalTargetError> {
        let resolved = self.resolve_terminal_target(target)?;
        self.state
            .workspaces
            .get(resolved.ws_idx)
            .and_then(|ws| ws.terminal_id(resolved.pane_id))
            .cloned()
            .ok_or_else(|| TerminalTargetError::NotFound {
                target: target.to_string(),
            })
    }

    fn terminal_display_name(&self, terminal: &TerminalId) -> String {
        self.state
            .terminals
            .get(terminal)
            .and_then(|state| state.agent_name.clone())
            .or_else(|| self.public_pane_id_of_terminal(terminal))
            .unwrap_or_else(|| terminal.to_string())
    }

    /// Records that a person used a pane, and what their keys do to the unsent text in its input.
    pub(crate) fn note_pane_keys(&mut self, terminal: &TerminalId, keys: &[deliveries::DraftKey]) {
        self.orchestration
            .activity
            .insert(terminal.clone(), Instant::now());
        if keys.is_empty() {
            return;
        }
        // Keys typed at a dialog answer the dialog. They are not text in the input.
        let at_dialog = self
            .state
            .terminals
            .get(terminal)
            .is_some_and(|state| state.state == AgentState::Blocked);
        if at_dialog {
            return;
        }
        if self
            .orchestration
            .drafts
            .apply(terminal, keys, Instant::now())
        {
            self.orchestration.wake();
        }
    }

    /// How far a terminal is scrolled back from the bottom of its output.
    fn scroll_offset(&self, ws_idx: usize, pane_id: crate::layout::PaneId) -> usize {
        self.state
            .runtime_for_pane_in_workspace(&self.terminal_runtimes, ws_idx, pane_id)
            .and_then(|runtime| runtime.scroll_metrics())
            .map_or(0, |metrics| metrics.offset_from_bottom)
    }

    /// Notes activity in each terminal that came into view or scrolled since the last look: the
    /// focused pane of an attached client, and the terminals a client is attached to alone.
    fn observe_in_view(&mut self, now: Instant) {
        let mut seen: Vec<TerminalId> = self
            .orchestration
            .attached_terminals
            .iter()
            .cloned()
            .collect();
        if self.orchestration.client_attached {
            if let Some(terminal) = self.state.active.and_then(|ws_idx| {
                let ws = self.state.workspaces.get(ws_idx)?;
                ws.terminal_id(ws.focused_pane_id()?).cloned()
            }) {
                seen.push(terminal);
            }
        }
        let in_view: HashMap<TerminalId, usize> = seen
            .into_iter()
            .filter_map(|terminal| {
                let (ws_idx, pane_id) = self.terminal_location(&terminal)?;
                Some((terminal, self.scroll_offset(ws_idx, pane_id)))
            })
            .collect();
        for (terminal, offset) in &in_view {
            if self.orchestration.in_view.get(terminal) != Some(offset) {
                self.orchestration.activity.insert(terminal.clone(), now);
            }
        }
        self.orchestration.in_view = in_view;
    }

    fn pane_readiness(&self, terminal: &TerminalId) -> Option<PaneReadiness> {
        let (ws_idx, pane_id) = self.terminal_location(terminal)?;
        let state = self.state.terminals.get(terminal)?;
        let ws = self.state.workspaces.get(ws_idx)?;
        let tab_idx = ws.find_tab_index_for_pane(pane_id)?;
        let focused = self.state.active == Some(ws_idx)
            && ws.active_tab == tab_idx
            && ws.focused_pane_id() == Some(pane_id);
        let last = self.orchestration.activity.get(terminal).copied();
        let now = Instant::now();
        let within =
            |hold: Duration| last.is_some_and(|at| now.saturating_duration_since(at) < hold);
        Some(PaneReadiness {
            agent_ready: state.effective_known_agent().is_some()
                && !state.managed_agent_launch_pending(),
            waiting_for_input: state.state == AgentState::Idle,
            unsent_input: self.orchestration.drafts.holds(terminal, Instant::now()),
            focused,
            client_attached: self.orchestration.client_attached,
            attached_directly: self.orchestration.attached_terminals.contains(terminal),
            recent_input: self.orchestration.focus_hold.is_none_or(&within),
            scrolled_back: self.scroll_offset(ws_idx, pane_id) > 0
                && within(deliveries::DRAFT_HOLD),
        })
    }

    fn delivery_info(&self, delivery: &Delivery, state: DeliveryState) -> DeliveryInfo {
        DeliveryInfo {
            delivery_id: delivery.id.clone(),
            pane_id: self
                .public_pane_id_of_terminal(&delivery.terminal)
                .unwrap_or_default(),
            text: delivery.text.clone(),
            source: delivery.source.clone(),
            state,
            queued_at_ms: delivery.queued_at_ms,
            held_by: (state == DeliveryState::Queued)
                .then(|| self.pane_readiness(&delivery.terminal))
                .flatten()
                .and_then(PaneReadiness::hold)
                .map(str::to_string),
        }
    }

    fn emit_delivery_changed(&mut self, terminal: &TerminalId) {
        let Some((ws_idx, pane_id)) = self.terminal_location(terminal) else {
            return;
        };
        let Some(pane_id) = self.public_pane_id(ws_idx, pane_id) else {
            return;
        };
        self.emit_event(EventEnvelope {
            event: EventKind::PaneDeliveryChanged,
            data: EventData::PaneDeliveryChanged {
                pane_id,
                workspace_id: self.public_workspace_id(ws_idx),
                pending: self.orchestration.pending_for(terminal),
            },
        });
    }

    /// Queues a prompt for a pane and types it at once when the pane is safe.
    pub(crate) fn enqueue_delivery(
        &mut self,
        terminal: TerminalId,
        text: String,
        source: Option<String>,
        ttl_ms: Option<u64>,
    ) -> Result<(Delivery, DeliveryState), DeliveryRefusal> {
        // Every delivery passes here, so text from the API, a job or a report is filtered once.
        let text = deliveries::sanitize(&text);
        if text.trim().is_empty() {
            return Err(DeliveryRefusal::Empty);
        }
        if !self.orchestration.deliveries.has_room(&terminal) {
            return Err(DeliveryRefusal::QueueFull);
        }
        let delivery =
            self.orchestration
                .deliveries
                .push(terminal.clone(), text, source, now_ms(), ttl_ms);
        self.orchestration.dirty = true;
        self.observe_in_view(Instant::now());
        self.drain_terminal_deliveries(&terminal);
        let state = if self
            .orchestration
            .deliveries
            .iter()
            .any(|queued| queued.id == delivery.id)
        {
            DeliveryState::Queued
        } else {
            DeliveryState::Delivered
        };
        self.emit_delivery_changed(&terminal);
        Ok((delivery, state))
    }

    /// Queues a line the server itself made. A refusal is logged, because nobody waits for it.
    /// True when the line was queued.
    fn enqueue_server_delivery(
        &mut self,
        terminal: TerminalId,
        text: String,
        source: String,
    ) -> bool {
        match self.enqueue_delivery(terminal, text, Some(source.clone()), None) {
            Ok(_) => true,
            Err(refusal) => {
                tracing::warn!(source = %source, reason = ?refusal, "delivery was not queued");
                false
            }
        }
    }

    /// Types the oldest delivery of a terminal when the pane is safe and none is being written.
    fn drain_terminal_deliveries(&mut self, terminal: &TerminalId) -> bool {
        if self.orchestration.in_flight.contains_key(terminal) {
            return false;
        }
        if !self
            .pane_readiness(terminal)
            .is_some_and(PaneReadiness::safe)
        {
            return false;
        }
        let Some((ws_idx, pane_id)) = self.terminal_location(terminal) else {
            return false;
        };
        let Some(expected_agent) = self
            .state
            .terminals
            .get(terminal)
            .and_then(|state| state.effective_known_agent())
        else {
            return false;
        };
        let Some(delivery) = self.orchestration.deliveries.take_next(terminal) else {
            return false;
        };
        match self.submit_text_to_agent(ws_idx, pane_id, expected_agent, &delivery.text, None) {
            Ok(completion) => {
                self.orchestration.in_flight.insert(
                    terminal.clone(),
                    InFlight {
                        delivery,
                        completion,
                    },
                );
                self.orchestration.dirty = true;
                true
            }
            Err(_) => {
                self.orchestration.deliveries.restore_front(delivery);
                false
            }
        }
    }

    fn settle_in_flight_deliveries(&mut self) -> Vec<TerminalId> {
        let mut settled = Vec::new();
        let mut failed = Vec::new();
        for (terminal, in_flight) in &self.orchestration.in_flight {
            match in_flight.completion.try_recv() {
                Ok(Ok(())) => settled.push(terminal.clone()),
                Ok(Err(_)) | Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    failed.push(terminal.clone())
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
            }
        }
        for terminal in &settled {
            self.orchestration.in_flight.remove(terminal);
            // A written delivery leaves the saved state, so a restart does not type it again.
            self.orchestration.dirty = true;
        }
        for terminal in &failed {
            if let Some(in_flight) = self.orchestration.in_flight.remove(terminal) {
                tracing::warn!(
                    delivery = %in_flight.delivery.id,
                    "delivery was not written; it stays queued"
                );
                self.orchestration
                    .deliveries
                    .restore_front(in_flight.delivery);
            }
        }
        settled
    }

    fn drain_deliveries(&mut self) {
        let mut changed = self.settle_in_flight_deliveries();
        for terminal in self.orchestration.deliveries.terminals() {
            if self.drain_terminal_deliveries(&terminal) {
                changed.push(terminal);
            }
        }
        changed.sort_by_key(|terminal| terminal.to_string());
        changed.dedup();
        for terminal in changed {
            self.emit_delivery_changed(&terminal);
        }
    }

    pub(crate) fn handle_agent_deliver(
        &mut self,
        id: String,
        params: AgentDeliverParams,
    ) -> String {
        self.ensure_orchestration_loaded();
        if params.text.trim().is_empty() {
            return encode_error(id, "empty_delivery", "delivery text must not be empty");
        }
        if params.text.len() > MAX_DELIVERY_BYTES {
            return encode_error(
                id,
                "delivery_too_large",
                format!("delivery text is limited to {MAX_DELIVERY_BYTES} bytes"),
            );
        }
        let terminal = match self.terminal_of_target(&params.target) {
            Ok(terminal) => terminal,
            Err(err) => return target_error(id, err),
        };
        let (delivery, state) =
            match self.enqueue_delivery(terminal, params.text, params.source, params.ttl_ms) {
                Ok(queued) => queued,
                Err(DeliveryRefusal::Empty) => {
                    return encode_error(id, "empty_delivery", "delivery text must not be empty");
                }
                Err(DeliveryRefusal::QueueFull) => {
                    return encode_error(
                        id,
                        "delivery_queue_full",
                        format!(
                            "{} deliveries already wait for this pane",
                            deliveries::MAX_PENDING_PER_TERMINAL
                        ),
                    );
                }
            };
        self.save_orchestration_if_dirty();
        encode_success(
            id,
            ResponseResult::Delivery {
                delivery: self.delivery_info(&delivery, state),
            },
        )
    }

    pub(crate) fn handle_agent_deliveries(
        &mut self,
        id: String,
        params: AgentDeliveriesParams,
    ) -> String {
        self.ensure_orchestration_loaded();
        let only = match params.target.as_deref().map(|t| self.terminal_of_target(t)) {
            Some(Ok(terminal)) => Some(terminal),
            Some(Err(err)) => return target_error(id, err),
            None => None,
        };
        let deliveries = self
            .orchestration
            .deliveries
            .iter()
            .filter(|delivery| only.as_ref().is_none_or(|only| &delivery.terminal == only))
            .map(|delivery| self.delivery_info(delivery, DeliveryState::Queued))
            .collect();
        encode_success(id, ResponseResult::DeliveryList { deliveries })
    }

    pub(crate) fn handle_agent_cancel_delivery(
        &mut self,
        id: String,
        params: AgentCancelDeliveryParams,
    ) -> String {
        self.ensure_orchestration_loaded();
        let Some(delivery) = self.orchestration.deliveries.cancel(&params.delivery_id) else {
            return encode_error(
                id,
                "delivery_not_found",
                format!("delivery {} is not queued", params.delivery_id),
            );
        };
        self.orchestration.dirty = true;
        self.emit_delivery_changed(&delivery.terminal);
        self.save_orchestration_if_dirty();
        encode_success(id, ResponseResult::Ok {})
    }

    pub(crate) fn handle_pane_supervise(
        &mut self,
        id: String,
        params: PaneSuperviseParams,
    ) -> String {
        self.ensure_orchestration_loaded();
        let pane = match self.terminal_of_target(&params.pane_id) {
            Ok(terminal) => terminal,
            Err(err) => return target_error(id, err),
        };
        match params.supervisor.as_deref() {
            None => {
                self.orchestration.links.remove(&pane);
                self.orchestration.last_seen.remove(&pane);
            }
            Some(target) => {
                let supervisor = match self.terminal_of_target(target) {
                    Ok(terminal) => terminal,
                    Err(err) => return target_error(id, err),
                };
                if supervision::would_cycle(&self.orchestration.links, &pane, &supervisor) {
                    return encode_error(
                        id,
                        "supervision_cycle",
                        format!("{target} cannot supervise {}", params.pane_id),
                    );
                }
                let mut report = params.report;
                report.dedup();
                let link = Supervision { supervisor, report };
                let previous = self.orchestration.links.insert(pane.clone(), link.clone());
                self.remember_supervised(&pane);
                // A worker that blocked before its link asked for it is reported now. The same
                // link set again is not a new block.
                let told = previous.is_some_and(|previous| {
                    previous.supervisor == link.supervisor
                        && previous.reports(SuperviseReport::Blocked)
                });
                let blocked = self
                    .state
                    .terminals
                    .get(&pane)
                    .is_some_and(|state| state.state == AgentState::Blocked);
                if blocked && !told && link.reports(SuperviseReport::Blocked) {
                    let blocker = self
                        .orchestration
                        .blockers
                        .get(&pane)
                        .and_then(|blocker| blocker.text.clone());
                    self.report_to_supervisor(&pane, SuperviseReport::Blocked, blocker.as_deref());
                }
            }
        }
        self.orchestration.dirty = true;
        self.save_orchestration_if_dirty();
        if let Some((ws_idx, pane_id)) = self.terminal_location(&pane) {
            self.emit_pane_updated(ws_idx, pane_id);
        }
        encode_success(id, ResponseResult::Ok {})
    }

    fn remember_supervised(&mut self, terminal: &TerminalId) {
        let Some(pane_id) = self.public_pane_id_of_terminal(terminal) else {
            return;
        };
        let had_agent = self
            .state
            .terminals
            .get(terminal)
            .is_some_and(|state| state.is_agent_terminal());
        let name = self.terminal_display_name(terminal);
        let previous = self.orchestration.last_seen.insert(
            terminal.clone(),
            LastSeen {
                name,
                pane_id,
                had_agent,
            },
        );
        // An agent that was seen once stays counted, so its exit is reported one time.
        if previous.is_some_and(|previous| previous.had_agent) {
            if let Some(seen) = self.orchestration.last_seen.get_mut(terminal) {
                seen.had_agent = true;
            }
        }
    }

    /// Sends one report line to the supervisor of a pane, when the link asks for that kind.
    fn report_to_supervisor(
        &mut self,
        terminal: &TerminalId,
        kind: SuperviseReport,
        detail: Option<&str>,
    ) {
        let Some(link) = self.orchestration.links.get(terminal).cloned() else {
            return;
        };
        if !link.reports(kind) || self.terminal_location(&link.supervisor).is_none() {
            return;
        }
        let (name, pane_id) = match self.orchestration.last_seen.get(terminal) {
            Some(seen) => (seen.name.clone(), seen.pane_id.clone()),
            None => (
                self.terminal_display_name(terminal),
                self.public_pane_id_of_terminal(terminal)
                    .unwrap_or_default(),
            ),
        };
        // A report that still waits is out of date once a newer one of its kind exists, and
        // every report about a pane is out of date once the pane is gone. Dropping those keeps
        // a supervisor that is slow to read from losing the report that matters.
        let replaced: &[&str] = match kind {
            SuperviseReport::Blocked => &["blocked"],
            SuperviseReport::Stalled => &["stalled"],
            SuperviseReport::Exited => &["blocked", "stalled", "exited"],
        };
        let heads: Vec<String> = replaced
            .iter()
            .map(|kind| supervision::report_line(kind, &name, &pane_id, None))
            .collect();
        let dropped = self
            .orchestration
            .deliveries
            .remove_where(&link.supervisor, |delivery| {
                delivery.source.as_deref() == Some(SUPERVISE_SOURCE)
                    && heads.iter().any(|head| {
                        delivery
                            .text
                            .strip_prefix(head.as_str())
                            .is_some_and(|rest| rest.is_empty() || rest.starts_with('|'))
                    })
            });
        if dropped > 0 {
            self.orchestration.dirty = true;
        }
        let kind_name = match kind {
            SuperviseReport::Blocked => "blocked",
            SuperviseReport::Exited => "exited",
            SuperviseReport::Stalled => "stalled",
        };
        let line = supervision::report_line(kind_name, &name, &pane_id, detail);
        let supervisor_pane_id = self
            .public_pane_id_of_terminal(&link.supervisor)
            .unwrap_or_default();
        self.enqueue_server_delivery(link.supervisor, line, SUPERVISE_SOURCE.to_string());
        // The same report for jobs, plugin hooks and history readers.
        self.emit_event(EventEnvelope {
            event: EventKind::PaneSuperviseReport,
            data: EventData::PaneSuperviseReport {
                pane_id,
                supervisor_pane_id,
                agent: name,
                kind,
                detail: detail.map(str::to_owned),
            },
        });
    }

    /// Records what a newly blocked agent waits for and tells its supervisor.
    pub(crate) fn note_agent_state_change(
        &mut self,
        ws_idx: usize,
        pane_id: crate::layout::PaneId,
        previous: AgentState,
        current: AgentState,
    ) -> Option<String> {
        let terminal = self
            .state
            .workspaces
            .get(ws_idx)
            .and_then(|ws| ws.terminal_id(pane_id))
            .cloned()?;
        self.orchestration.wake();
        // An agent that starts to work took what was in its input. Text typed while it works
        // is a new draft, so only the start of the work clears.
        if current == AgentState::Working && previous != AgentState::Working {
            self.orchestration.drafts.clear(&terminal);
        }
        if current != AgentState::Blocked {
            self.orchestration.blockers.remove(&terminal);
            return None;
        }
        if previous == AgentState::Blocked {
            return self
                .orchestration
                .blockers
                .get(&terminal)
                .and_then(|blocker| blocker.text.clone());
        }
        self.orchestration.block_count += 1;
        let blocker = self
            .state
            .terminals
            .get(&terminal)
            .and_then(|state| state.hook_authority.as_ref())
            .and_then(|authority| authority.message.as_deref())
            .map(supervision::Blocker::from_message)
            .filter(|blocker| blocker.text.is_some())
            .or_else(|| self.read_blocker(&terminal, self.orchestration.block_count));
        let text = blocker.as_ref().and_then(|blocker| blocker.text.clone());
        match blocker {
            Some(blocker) => {
                self.orchestration
                    .blockers
                    .insert(terminal.clone(), blocker);
            }
            None => {
                self.orchestration.blockers.remove(&terminal);
            }
        }
        self.ensure_orchestration_loaded();
        self.report_to_supervisor(&terminal, SuperviseReport::Blocked, text.as_deref());
        text
    }

    /// Reads what the agent in a terminal shows now.
    fn read_blocker(&self, terminal: &TerminalId, block: u64) -> Option<supervision::Blocker> {
        let (ws_idx, pane_id) = self.terminal_location(terminal)?;
        let runtime = self.lookup_runtime_sender(ws_idx, pane_id)?;
        // The counter is read first: bytes that come between the two reads then leave a counter
        // that is behind the text, and the next refresh reads the screen again.
        let seq = runtime.content_seq();
        Some(supervision::Blocker::from_screen(
            &runtime.detection_text(),
            seq,
            block,
        ))
    }

    /// Reads the screen of each blocked agent again when it changed, so that the record follows
    /// an agent that goes from one dialog to the next with no state change in between.
    fn refresh_blockers(&mut self) {
        let stale: Vec<TerminalId> = self
            .orchestration
            .blockers
            .iter()
            .filter(|(terminal, blocker)| {
                blocker.seq.is_some_and(|seq| {
                    self.terminal_location(terminal)
                        .and_then(|(ws_idx, pane_id)| self.lookup_runtime_sender(ws_idx, pane_id))
                        .is_some_and(|runtime| runtime.content_seq() != seq)
                })
            })
            .map(|(terminal, _)| terminal.clone())
            .collect();
        for terminal in stale {
            let Some(block) = self
                .orchestration
                .blockers
                .get(&terminal)
                .map(|blocker| blocker.block)
            else {
                continue;
            };
            if let Some(blocker) = self.read_blocker(&terminal, block) {
                self.orchestration.blockers.insert(terminal, blocker);
            }
        }
    }

    /// The name of the dialog a blocked agent shows now, read from its screen at this moment.
    pub(crate) fn current_dialog(&self, terminal: &TerminalId) -> Option<String> {
        let blocked = self
            .state
            .terminals
            .get(terminal)
            .is_some_and(|state| state.state == AgentState::Blocked);
        let block = self.orchestration.blockers.get(terminal)?.block;
        blocked
            .then(|| self.read_blocker(terminal, block))
            .flatten()
            .and_then(|blocker| blocker.dialog())
    }

    /// Drops state of panes that are gone and reports the ones whose agent went away.
    fn sweep_gone_terminals(&mut self) {
        let supervised: Vec<TerminalId> = self.orchestration.links.keys().cloned().collect();
        for terminal in supervised {
            let present = self.terminal_location(&terminal).is_some();
            let has_agent = present
                && self
                    .state
                    .terminals
                    .get(&terminal)
                    .is_some_and(|state| state.is_agent_terminal());
            let had_agent = self
                .orchestration
                .last_seen
                .get(&terminal)
                .is_some_and(|seen| seen.had_agent);
            if had_agent && !has_agent {
                self.report_to_supervisor(&terminal, SuperviseReport::Exited, None);
                if let Some(seen) = self.orchestration.last_seen.get_mut(&terminal) {
                    seen.had_agent = false;
                }
            }
            if present {
                if has_agent || !had_agent {
                    self.remember_supervised(&terminal);
                }
            } else {
                self.orchestration.links.remove(&terminal);
                self.orchestration.last_seen.remove(&terminal);
                self.orchestration.dirty = true;
            }
        }

        let orphaned: Vec<TerminalId> = self
            .orchestration
            .links
            .iter()
            .filter(|(_, link)| self.terminal_location(&link.supervisor).is_none())
            .map(|(terminal, _)| terminal.clone())
            .collect();
        for terminal in orphaned {
            self.orchestration.links.remove(&terminal);
            self.orchestration.last_seen.remove(&terminal);
            self.orchestration.dirty = true;
        }

        for terminal in self.orchestration.deliveries.terminals() {
            if self.terminal_location(&terminal).is_none()
                && !self
                    .orchestration
                    .deliveries
                    .drop_terminal(&terminal)
                    .is_empty()
            {
                self.orchestration.dirty = true;
            }
        }
        let gone: Vec<TerminalId> = self
            .orchestration
            .in_flight
            .keys()
            .chain(self.orchestration.blockers.keys())
            .filter(|terminal| self.terminal_location(terminal).is_none())
            .cloned()
            .collect();
        for terminal in gone {
            self.orchestration.in_flight.remove(&terminal);
            self.orchestration.blockers.remove(&terminal);
        }
        let mut drafts = std::mem::take(&mut self.orchestration.drafts);
        drafts.retain(|terminal| self.terminal_location(terminal).is_some());
        self.orchestration.drafts = drafts;
        let mut activity = std::mem::take(&mut self.orchestration.activity);
        activity.retain(|terminal, _| self.terminal_location(terminal).is_some());
        self.orchestration.activity = activity;
    }

    fn sample_stalls(&mut self, now: Instant) {
        let mut samples: Vec<(TerminalId, bool, u64)> = self
            .state
            .terminals
            .iter()
            .filter(|(_, state)| state.state == AgentState::Working && state.is_agent_terminal())
            .map(|(terminal, _)| {
                let seq = self
                    .terminal_location(terminal)
                    .and_then(|(ws_idx, pane_id)| self.lookup_runtime_sender(ws_idx, pane_id))
                    .map(|runtime| runtime.content_seq())
                    .unwrap_or(0);
                (terminal.clone(), true, seq)
            })
            .collect();
        for terminal in self.orchestration.stalls.watched() {
            if !samples.iter().any(|(sampled, _, _)| sampled == &terminal) {
                samples.push((terminal, false, 0));
            }
        }
        let limit = self.orchestration.stall_after;
        for (terminal, working, seq) in samples {
            let Some(change) = self
                .orchestration
                .stalls
                .observe(&terminal, working, seq, now, limit)
            else {
                continue;
            };
            let (stalled, quiet_secs) = match change {
                StallChange::Entered { quiet_secs } => (true, quiet_secs),
                StallChange::Left { quiet_secs } => (false, quiet_secs),
            };
            if let Some((ws_idx, pane_id)) = self.terminal_location(&terminal) {
                if let Some(pane_id) = self.public_pane_id(ws_idx, pane_id) {
                    self.emit_event(EventEnvelope {
                        event: EventKind::PaneAgentStalled,
                        data: EventData::PaneAgentStalled {
                            pane_id,
                            workspace_id: self.public_workspace_id(ws_idx),
                            stalled,
                            quiet_secs,
                        },
                    });
                }
            }
            if stalled {
                self.report_to_supervisor(
                    &terminal,
                    SuperviseReport::Stalled,
                    Some(&quiet_secs.to_string()),
                );
            }
        }
    }

    /// The periodic pass: drops what is gone or expired, types what is safe, samples for stalls.
    pub(crate) fn run_orchestration_tick(&mut self, now: Instant) {
        if !self.orchestration.tick_due(now) {
            return;
        }
        self.orchestration.next_tick = Some(now + TICK);
        self.ensure_orchestration_loaded();
        self.sweep_gone_terminals();
        self.observe_in_view(now);
        for expired in self.orchestration.deliveries.expire(now_ms()) {
            self.orchestration.dirty = true;
            self.emit_delivery_changed(&expired.terminal);
        }
        self.sample_stalls(now);
        self.refresh_blockers();
        self.run_due_jobs(now);
        self.drain_deliveries();
        self.save_orchestration_if_dirty();
    }

    pub(crate) fn orchestration_agent_fields(
        &self,
        terminal: &TerminalId,
    ) -> AgentOrchestrationFields {
        let supervisor = self
            .orchestration
            .links
            .get(terminal)
            .and_then(|link| self.public_pane_id_of_terminal(&link.supervisor));
        let pending = Some(self.orchestration.pending_for(terminal)).filter(|count| *count > 0);
        let stalled = self
            .orchestration
            .stalls
            .stalled_secs(terminal, Instant::now());
        let blocker = self.orchestration.blockers.get(terminal);
        AgentOrchestrationFields {
            supervisor_pane_id: supervisor,
            pending_deliveries: pending,
            stalled_secs: stalled,
            blocker: blocker.and_then(|blocker| blocker.text.clone()),
            blocker_lines: blocker
                .map(|blocker| blocker.lines.clone())
                .unwrap_or_default(),
            dialog: blocker.and_then(|blocker| blocker.dialog()),
        }
    }

    fn stored_orchestration_state(&self) -> store::StoredState {
        let pane = |terminal: &TerminalId| self.public_pane_id_of_terminal(terminal);
        let deliveries = self
            .orchestration
            .in_flight
            .values()
            .map(|in_flight| &in_flight.delivery)
            .chain(self.orchestration.deliveries.iter())
            .filter_map(|delivery| {
                Some(store::StoredDelivery {
                    pane_id: pane(&delivery.terminal)?,
                    text: delivery.text.clone(),
                    source: delivery.source.clone(),
                    queued_at_ms: delivery.queued_at_ms,
                    expires_at_ms: delivery.expires_at_ms,
                })
            })
            .collect();
        let mut links: Vec<store::StoredLink> = self
            .orchestration
            .links
            .iter()
            .filter_map(|(terminal, link)| {
                Some(store::StoredLink {
                    pane_id: pane(terminal)?,
                    supervisor_pane_id: pane(&link.supervisor)?,
                    report: link.report.clone(),
                })
            })
            .collect();
        links.sort_by(|a, b| a.pane_id.cmp(&b.pane_id));
        let jobs = self
            .orchestration
            .jobs
            .iter()
            .filter_map(|job| {
                let spec = &job.spec;
                // A job whose pane cannot be named would come back without it and never end.
                let target_pane_id = match &spec.target {
                    Some(target) => Some(pane(target)?),
                    None => None,
                };
                let owner_pane_id = match &spec.owner {
                    Some(owner) => Some(pane(owner)?),
                    None => None,
                };
                Some(store::StoredJob {
                    name: spec.name.clone(),
                    command: spec.command.clone(),
                    cwd: spec.cwd.clone(),
                    env: spec.env.clone(),
                    every_ms: spec.every.map(|every| every.as_millis() as u64),
                    on_events: spec.on_events.clone(),
                    on_path: spec.on_path.as_ref().map(|path| path.display().to_string()),
                    target_pane_id,
                    owner_pane_id,
                    timeout_ms: spec.timeout.as_millis() as u64,
                    until_ms: spec.until_ms,
                    dedupe: spec.dedupe,
                    last_news: job.last_news.clone(),
                })
            })
            .collect();
        let mut tokens: Vec<store::StoredTokens> = self
            .state
            .terminals
            .iter()
            .filter_map(|(terminal, state)| {
                let tokens = state.metadata_tokens.lasting_values();
                (!tokens.is_empty()).then_some(())?;
                Some(store::StoredTokens {
                    pane_id: pane(terminal)?,
                    tokens,
                })
            })
            .collect();
        tokens.sort_by(|a, b| a.pane_id.cmp(&b.pane_id));
        store::StoredState {
            version: 0,
            deliveries,
            links,
            jobs,
            tokens,
        }
    }

    /// Saves changed state, at most once in [`SAVE_INTERVAL`]. A change inside that time stays
    /// marked and is saved by a later tick, so a burst of changes is one write.
    pub(crate) fn save_orchestration_if_dirty(&mut self) {
        let recent = self
            .orchestration
            .last_save
            .is_some_and(|at| at.elapsed() < SAVE_INTERVAL);
        if self.orchestration.dirty && recent {
            // The tick saves it. A tick that is already nearer than the save time stays.
            let due = self.orchestration.last_save.map(|at| at + SAVE_INTERVAL);
            if let (Some(due), Some(next)) = (due, self.orchestration.next_tick) {
                self.orchestration.next_tick = Some(next.min(due));
            }
            return;
        }
        self.save_orchestration_now();
    }

    /// Stops what must not outlive this server and saves the state. For the end of the server
    /// and for a handoff: the next server starts the jobs again, and a command left running here
    /// would have nobody to stop it at its time limit.
    pub(crate) fn close_orchestration(&mut self) {
        for group in self.orchestration.jobs.stop_all() {
            crate::platform::kill_process_group_id(group);
        }
        self.save_orchestration_now();
    }

    /// Saves changed state at once.
    pub(crate) fn save_orchestration_now(&mut self) {
        if !std::mem::take(&mut self.orchestration.dirty) {
            return;
        }
        self.orchestration.last_save = Some(Instant::now());
        let Some(path) = self.orchestration.store_path.clone() else {
            return;
        };
        if let Err(err) = store::save(&path, &self.stored_orchestration_state()) {
            tracing::warn!(error = %err, path = %path.display(), "failed to save orchestration state");
        }
    }

    /// Reads the state file one time, after the panes it names exist.
    pub(crate) fn ensure_orchestration_loaded(&mut self) {
        if std::mem::replace(&mut self.orchestration.loaded, true) {
            return;
        }
        let Some(path) = self.orchestration.store_path.clone() else {
            return;
        };
        let stored = store::load(&path);
        let terminal = |app: &App, pane_id: &str| {
            let (ws_idx, pane) = app.parse_pane_id(pane_id)?;
            app.state.workspaces.get(ws_idx)?.terminal_id(pane).cloned()
        };
        for delivery in stored.deliveries {
            let Some(target) = terminal(self, &delivery.pane_id) else {
                continue;
            };
            // A saved file gets the same checks as a request.
            let text = deliveries::sanitize(&delivery.text);
            if text.trim().is_empty() || !self.orchestration.deliveries.has_room(&target) {
                continue;
            }
            let mut restored = self.orchestration.deliveries.push(
                target,
                text,
                delivery.source,
                delivery.queued_at_ms,
                None,
            );
            self.orchestration.deliveries.cancel(&restored.id);
            restored.expires_at_ms = delivery.expires_at_ms;
            self.orchestration.deliveries.restore(restored);
        }
        for link in stored.links {
            let (Some(pane), Some(supervisor)) = (
                terminal(self, &link.pane_id),
                terminal(self, &link.supervisor_pane_id),
            ) else {
                continue;
            };
            if supervision::would_cycle(&self.orchestration.links, &pane, &supervisor) {
                continue;
            }
            self.orchestration.links.insert(
                pane.clone(),
                Supervision {
                    supervisor,
                    report: link.report,
                },
            );
            self.remember_supervised(&pane);
        }
        let now = Instant::now();
        for saved in stored.tokens {
            let Some(target) = terminal(self, &saved.pane_id) else {
                continue;
            };
            let Some(state) = self.state.terminals.get_mut(&target) else {
                continue;
            };
            // A token set since the start is newer than the saved one.
            let current = state.metadata_tokens.values();
            let patch = saved
                .tokens
                .into_iter()
                .filter(|(key, _)| !current.contains_key(key))
                .map(|(key, value)| (key, Some(value)))
                .collect();
            if state.metadata_tokens.patch(patch, None, now) {
                state.revision = state.revision.saturating_add(1);
            }
        }
        for job in stored.jobs {
            let target = match job.target_pane_id.as_deref() {
                Some(pane_id) => match terminal(self, pane_id) {
                    Some(target) => Some(target),
                    None => continue,
                },
                None => None,
            };
            let owner = match job.owner_pane_id.as_deref() {
                Some(pane_id) => match terminal(self, pane_id) {
                    Some(owner) => Some(owner),
                    None => continue,
                },
                None => None,
            };
            let spec = JobSpec {
                name: job.name,
                command: job.command,
                cwd: job.cwd,
                env: job.env,
                every: job.every_ms.map(Duration::from_millis),
                on_events: job.on_events,
                on_path: job.on_path.map(PathBuf::from),
                target,
                owner,
                timeout: Duration::from_millis(job.timeout_ms),
                until_ms: job.until_ms,
                dedupe: job.dedupe,
            };
            let hookable = crate::api::schema::plugin_hook_event_names();
            let Some(spec) = spec.checked_as_saved(&hookable) else {
                tracing::warn!("a saved job that no request could make was dropped");
                continue;
            };
            if self.orchestration.jobs.len() < jobs::MAX_JOBS {
                self.orchestration.jobs.add(spec, job.last_news, now);
            }
        }
    }

    fn job_info(&self, job: &jobs::Job) -> JobInfo {
        let spec = &job.spec;
        JobInfo {
            name: spec.name.clone(),
            command: spec.command.clone(),
            cwd: spec.cwd.clone(),
            every_ms: spec.every.map(|every| every.as_millis() as u64),
            on_events: spec.on_events.clone(),
            on_path: spec.on_path.as_ref().map(|path| path.display().to_string()),
            target_pane_id: spec
                .target
                .as_ref()
                .and_then(|target| self.public_pane_id_of_terminal(target)),
            owner_pane_id: spec
                .owner
                .as_ref()
                .and_then(|owner| self.public_pane_id_of_terminal(owner)),
            timeout_ms: spec.timeout.as_millis() as u64,
            until_ms: spec.until_ms,
            dedupe: spec.dedupe,
            running: job.running.is_some(),
            runs: job.runs,
            last_exit: job.last_exit,
            last_run_ms: job.last_run_ms,
            last_news: job.last_news.clone(),
        }
    }

    pub(crate) fn handle_job_add(&mut self, id: String, params: JobAddParams) -> String {
        self.ensure_orchestration_loaded();
        let hookable = crate::api::schema::plugin_hook_event_names();
        if let Err((code, message)) = jobs::validate(&params, &hookable) {
            return encode_error(id, code, message);
        }
        if !self.orchestration.jobs.contains(&params.name)
            && self.orchestration.jobs.len() >= jobs::MAX_JOBS
        {
            return encode_error(
                id,
                "job_limit_reached",
                format!("at most {} jobs can exist", jobs::MAX_JOBS),
            );
        }
        let target = match params.target.as_deref().map(|t| self.terminal_of_target(t)) {
            Some(Ok(terminal)) => Some(terminal),
            Some(Err(err)) => return target_error(id, err),
            None => None,
        };
        let owner = match params
            .owner_pane_id
            .as_deref()
            .map(|t| self.terminal_of_target(t))
        {
            Some(Ok(terminal)) => Some(terminal),
            Some(Err(err)) => return target_error(id, err),
            None => None,
        };
        let name = params.name.clone();
        self.orchestration.jobs.add(
            JobSpec {
                name: params.name,
                command: params.command,
                cwd: params.cwd,
                env: params.env.into_iter().collect(),
                every: params.every_ms.map(Duration::from_millis),
                on_events: params.on_events,
                on_path: params.on_path.map(PathBuf::from),
                target,
                owner,
                timeout: Duration::from_millis(
                    params.timeout_ms.unwrap_or(jobs::DEFAULT_TIMEOUT_MS),
                ),
                until_ms: params.until_ms,
                dedupe: params.dedupe.unwrap_or(true),
            },
            None,
            Instant::now(),
        );
        self.orchestration.dirty = true;
        self.orchestration.wake();
        self.save_orchestration_if_dirty();
        match self.orchestration.jobs.get(&name) {
            Some(job) => encode_success(
                id,
                ResponseResult::JobInfo {
                    job: self.job_info(job),
                },
            ),
            None => encode_error(id, "job_not_found", format!("job {name} not found")),
        }
    }

    pub(crate) fn handle_job_list(&mut self, id: String) -> String {
        self.ensure_orchestration_loaded();
        let jobs = self
            .orchestration
            .jobs
            .iter()
            .map(|job| self.job_info(job))
            .collect();
        encode_success(id, ResponseResult::JobList { jobs })
    }

    pub(crate) fn handle_job_remove(&mut self, id: String, params: JobTarget) -> String {
        self.ensure_orchestration_loaded();
        if self.orchestration.jobs.remove(&params.name).is_none() {
            return encode_error(
                id,
                "job_not_found",
                format!("job {} not found", params.name),
            );
        }
        self.orchestration.dirty = true;
        self.save_orchestration_if_dirty();
        encode_success(id, ResponseResult::Ok {})
    }

    pub(crate) fn handle_job_run(&mut self, id: String, params: JobTarget) -> String {
        self.ensure_orchestration_loaded();
        if !self.orchestration.jobs.contains(&params.name) {
            return encode_error(
                id,
                "job_not_found",
                format!("job {} not found", params.name),
            );
        }
        self.start_job(&params.name, None);
        match self.orchestration.jobs.get(&params.name) {
            Some(job) => encode_success(
                id,
                ResponseResult::JobInfo {
                    job: self.job_info(job),
                },
            ),
            None => encode_error(
                id,
                "job_not_found",
                format!("job {} not found", params.name),
            ),
        }
    }

    fn start_job(&mut self, name: &str, event: Option<TriggerEvent>) {
        let Some(run) = self.orchestration.jobs.begin(name, event, Instant::now()) else {
            return;
        };
        let target_pane_id = run
            .spec
            .target
            .as_ref()
            .and_then(|target| self.public_pane_id_of_terminal(target));
        spawn_job_run(run, target_pane_id, self.event_tx.clone());
    }

    /// Starts the jobs that run on this event.
    pub(crate) fn run_jobs_for_event(&mut self, event: &EventEnvelope) {
        let name = event.event.dot_name();
        let names = self.orchestration.jobs.names_for_event(name);
        if names.is_empty() {
            return;
        }
        let json = serde_json::to_string(event).unwrap_or_default();
        for job in names {
            self.start_job(
                &job,
                Some(TriggerEvent {
                    name: name.to_string(),
                    json: json.clone(),
                }),
            );
        }
    }

    fn run_due_jobs(&mut self, now: Instant) {
        let ended = self.orchestration.jobs.ended(now_ms(), |terminal| {
            self.terminal_location(terminal).is_none()
        });
        for name in ended {
            self.orchestration.jobs.remove(&name);
            self.orchestration.dirty = true;
        }
        let mut names = self.orchestration.jobs.take_due(now);
        names.extend(self.orchestration.jobs.take_changed_paths(|path| {
            let meta = std::fs::metadata(path).ok()?;
            Some((meta.modified().ok()?, meta.len()))
        }));
        names.dedup();
        for name in names {
            self.start_job(&name, None);
        }
        for (name, event) in self.orchestration.jobs.take_held(now) {
            self.start_job(&name, event);
        }
    }

    pub(crate) fn handle_job_finished(
        &mut self,
        name: &str,
        run_id: u64,
        exit: Option<i32>,
        stdout: &str,
    ) {
        let Some(outcome) = self
            .orchestration
            .jobs
            .finish(name, run_id, exit, stdout, now_ms())
        else {
            return;
        };
        if outcome.news {
            self.orchestration.dirty = true;
        }
        if let Some((target, text)) = outcome.deliver {
            // News counts as told only when it was queued, so that news a full queue refused
            // is delivered by a later run with the same output.
            if self.enqueue_server_delivery(target, text.clone(), format!("job:{name}")) {
                self.orchestration.jobs.note_news_delivered(name, text);
            }
        }
        // Not through `emit_event`: a job must not be able to run on its own end.
        self.event_hub.push(EventEnvelope {
            event: EventKind::JobFinished,
            data: EventData::JobFinished {
                name: name.to_string(),
                exit,
                news: outcome.news,
            },
        });
        // A trigger held during the run starts from the tick, which keeps the gap between runs.
        self.orchestration.wake();
        self.save_orchestration_if_dirty();
    }
}

const JOB_OUTPUT_MAX_BYTES: u64 = 64 * 1024;
const JOB_POLL: Duration = Duration::from_millis(50);
/// How long a run waits for its output to close after its command has exited.
const JOB_OUTPUT_GRACE: Duration = Duration::from_secs(2);

/// Runs one job command on its own thread and reports the end to the app.
fn spawn_job_run(
    run: JobRun,
    target_pane_id: Option<String>,
    event_tx: tokio::sync::mpsc::Sender<crate::events::AppEvent>,
) {
    std::thread::spawn(move || {
        let (exit_code, stdout) = run_job_command(&run, target_pane_id);
        let _ = event_tx.blocking_send(crate::events::AppEvent::JobFinished {
            name: run.spec.name,
            run_id: run.run_id,
            exit_code,
            stdout,
        });
    });
}

fn run_job_command(run: &JobRun, target_pane_id: Option<String>) -> (Option<i32>, String) {
    use std::io::Read;
    use std::process::Stdio;

    let spec = &run.spec;
    let Some((program, args)) = spec.command.split_first() else {
        return (None, String::new());
    };
    let cwd = match spec.cwd.as_deref().map(PathBuf::from) {
        Some(cwd) if cwd.is_dir() => cwd,
        // A command written for one folder must not run in another.
        Some(cwd) => {
            tracing::warn!(job = %spec.name, cwd = %cwd.display(), "job folder is gone; not run");
            return (None, String::new());
        }
        None => std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")),
    };
    let mut command = crate::plugin_command::command_for_argv_in_dir(program, args, &cwd);
    command
        .envs(&spec.env)
        .env("HERDR_ENV", "1")
        .env("HERDR_JOB_NAME", &spec.name)
        .env(
            crate::api::SOCKET_PATH_ENV_VAR,
            crate::api::socket_path().display().to_string(),
        )
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    if let Ok(exe) = crate::platform::launch_executable() {
        command.env("HERDR_BIN_PATH", exe);
    }
    if let Some(pane_id) = target_pane_id {
        command.env("HERDR_JOB_TARGET_PANE_ID", pane_id);
    }
    if let Some(event) = &run.event {
        command
            .env("HERDR_JOB_EVENT", &event.name)
            .env("HERDR_JOB_EVENT_JSON", &event.json);
    }
    crate::platform::isolate_process_group(&mut command);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(err) => {
            tracing::warn!(job = %spec.name, error = %err, "job command did not start");
            return (None, String::new());
        }
    };
    run.cancel.set_group(child.id());
    // The reader is never joined, because a process the command left behind can keep the output
    // open. What it has read is shared, so output printed before such a wait is not lost.
    let output = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let (closed_tx, closed_rx) = std::sync::mpsc::channel();
    if let Some(stdout) = child.stdout.take() {
        let output = output.clone();
        std::thread::spawn(move || {
            let mut stdout = stdout.take(JOB_OUTPUT_MAX_BYTES);
            let mut chunk = [0u8; 4096];
            while let Ok(count @ 1..) = stdout.read(&mut chunk) {
                if let Ok(mut output) = output.lock() {
                    output.extend_from_slice(&chunk[..count]);
                }
            }
            let _ = closed_tx.send(());
        });
    }
    let deadline = Instant::now() + spec.timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                // Whatever the command left running goes with it, which also closes the output.
                crate::platform::kill_process_group(&mut child);
                let _ = closed_rx.recv_timeout(JOB_OUTPUT_GRACE);
                let stdout = output
                    .lock()
                    .map(|output| String::from_utf8_lossy(&output).into_owned())
                    .unwrap_or_default();
                return (status.code(), stdout);
            }
            Ok(None) if run.cancel.is_set() => {
                crate::platform::kill_process_group(&mut child);
                let _ = child.wait();
                return (None, String::new());
            }
            Ok(None) if Instant::now() >= deadline => {
                tracing::warn!(job = %spec.name, "job command passed its time limit; stopped");
                crate::platform::kill_process_group(&mut child);
                let _ = child.wait();
                return (None, String::new());
            }
            Ok(None) => std::thread::sleep(JOB_POLL),
            Err(_) => {
                crate::platform::kill_process_group(&mut child);
                return (None, String::new());
            }
        }
    }
}
