//! Durable automation fences. Cooperative checkpoints, not detector states, confirm work stopped.
use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::{deliveries::Delivery, now_ms, store, App, TerminalId};
use crate::api::schema::{ExpectedSubtreeMember, Request, ResponseResult, SubtreeControlParams};
use crate::app::api::responses::{encode_error, encode_success};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(super) struct PauseJournal {
    #[serde(default)]
    version: u32,
    records: BTreeMap<String, PauseRecord>,
    #[serde(skip)]
    fault: Option<String>,
    #[serde(skip)]
    unreadable: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Member {
    pane_id: String,
    identity: Value,
    agent: Value,
    depth: usize,
    requested: bool,
    checkpoint: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PauseRecord {
    id: String,
    root: String,
    state: String,
    created_ms: u64,
    members: Vec<Member>,
    links: Vec<store::StoredLink>,
    #[serde(default)]
    observed_links: Vec<store::StoredLink>,
    #[serde(default)]
    expected: Vec<ExpectedSubtreeMember>,
    jobs: Vec<store::StoredJob>,
    deliveries: Vec<store::StoredDelivery>,
    quarantined: Vec<String>,
    restored_jobs: Vec<String>,
    authorization: Option<String>,
    #[serde(default)]
    reconciliations: Vec<Value>,
}

fn decode_journal(bytes: &[u8]) -> Result<PauseJournal, String> {
    let journal: PauseJournal = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
    if journal.version != 1 || journal.records.is_empty() {
        return Err("unsupported or incomplete pause journal".into());
    }
    for (id, record) in &journal.records {
        let members: HashSet<_> = record.members.iter().map(|m| m.pane_id.as_str()).collect();
        if id != &record.id
            || record.members.is_empty()
            || members.len() != record.members.len()
            || !members.contains(record.root.as_str())
            || !matches!(record.state.as_str(), "pausing" | "resumed" | "released")
        {
            return Err(format!("invalid pause record: {id}"));
        }
    }
    Ok(journal)
}

/// Loaded once before native session restore, before the App and its normal admission gate exist.
pub(crate) struct RestoreFence {
    panes: HashSet<String>,
    failed: bool,
}
impl RestoreFence {
    pub(crate) fn load(path: &std::path::Path) -> Self {
        let journal = match std::fs::read(path) {
            Ok(bytes) => decode_journal(&bytes),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(PauseJournal::default()),
            Err(e) => Err(e.to_string()),
        };
        match journal {
            Ok(j) => Self {
                panes: j
                    .records
                    .values()
                    .filter(|p| p.holds_automation())
                    .flat_map(|p| p.members.iter().map(|m| m.pane_id.clone()))
                    .collect(),
                failed: false,
            },
            Err(e) => {
                tracing::warn!(error=%e,"pause journal unreadable; automatic agent restore disabled");
                Self {
                    panes: HashSet::new(),
                    failed: true,
                }
            }
        }
    }
    pub(crate) fn holds(&self, pane: Option<&str>) -> bool {
        self.failed || pane.map_or(!self.panes.is_empty(), |p| self.panes.contains(p))
    }
}

impl PauseRecord {
    fn active(&self) -> bool {
        self.state != "resumed" && self.state != "released"
    }
    fn holds_automation(&self) -> bool {
        self.state != "resumed"
    }
    fn contains(&self, pane: &str) -> bool {
        self.members.iter().any(|m| m.pane_id == pane)
    }
    fn below(&self, child: &str, ancestor: &str) -> bool {
        let mut current = child;
        for _ in 0..self.members.len() {
            let Some(link) = self.links.iter().find(|link| link.pane_id == current) else {
                return false;
            };
            if link.supervisor_pane_id == ancestor {
                return true;
            }
            current = &link.supervisor_pane_id;
        }
        false
    }
}

impl App {
    fn pause_path(&self) -> Option<PathBuf> {
        self.orchestration
            .store_path
            .as_ref()
            .map(|p| p.with_file_name("subtree-pauses.json"))
    }

    pub(super) fn load_subtree_pauses(&mut self) {
        let Some(path) = self.pause_path() else {
            return;
        };
        let loaded = match std::fs::read(&path) {
            Ok(bytes) => decode_journal(&bytes),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
            Err(e) => Err(e.to_string()),
        };
        match loaded {
            Ok(journal) => self.orchestration.pauses = journal,
            Err(error) => {
                self.orchestration.pauses.unreadable = true;
                self.orchestration.pauses.fault = Some(format!("{}: {error}", path.display()))
            }
        }
    }

    fn save_subtree_pauses(&mut self) -> Result<(), String> {
        use std::io::Write;
        let Some(path) = self.pause_path() else {
            return Ok(());
        };
        self.orchestration.pauses.version = 1;
        let result = (|| -> std::io::Result<()> {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let bytes = serde_json::to_vec_pretty(&self.orchestration.pauses)?;
            let tmp = path.with_extension(format!("json.tmp-{}", std::process::id()));
            let _ = std::fs::remove_file(&tmp);
            let mut file = crate::platform::create_config_temporary(&tmp, true)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            std::fs::rename(tmp, &path)
        })();
        match result {
            Ok(()) => {
                self.orchestration.pauses.fault = None;
                Ok(())
            }
            Err(e) => {
                let error = e.to_string();
                self.orchestration.pauses.fault = Some(error.clone());
                Err(error)
            }
        }
    }

    fn pane_paused(&self, pane: &str) -> bool {
        self.orchestration.pauses.fault.is_some()
            || self
                .orchestration
                .pauses
                .records
                .values()
                .any(|p| p.active() && p.contains(pane))
    }

    pub(crate) fn subtree_terminal_paused(&self, terminal: &TerminalId) -> bool {
        self.orchestration.pauses.fault.is_some()
            || self
                .public_pane_id_of_terminal(terminal)
                .is_some_and(|p| self.pane_paused(&p))
    }

    pub(super) fn subtree_automation_held(&self, terminal: &TerminalId) -> bool {
        self.orchestration.pauses.fault.is_some()
            || self
                .public_pane_id_of_terminal(terminal)
                .is_some_and(|pane| {
                    self.orchestration
                        .pauses
                        .records
                        .values()
                        .any(|p| p.holds_automation() && p.contains(&pane))
                })
    }

    pub(super) fn subtree_job_definition_protected(&self, name: &str) -> bool {
        self.orchestration.pauses.fault.is_some()
            || self
                .orchestration
                .pauses
                .records
                .values()
                .any(|p| p.active() && p.jobs.iter().any(|j| j.name == name))
            || self.orchestration.jobs.get(name).is_some_and(|j| {
                j.spec
                    .target
                    .as_ref()
                    .is_some_and(|t| self.subtree_terminal_paused(t))
                    || j.spec
                        .owner
                        .as_ref()
                        .is_some_and(|t| self.subtree_terminal_paused(t))
            })
    }

    pub(super) fn subtree_delivery_quarantined(&self, terminal: &TerminalId) -> bool {
        self.orchestration
            .deliveries
            .iter()
            .find(|d| &d.terminal == terminal)
            .is_some_and(|d| self.subtree_delivery_id_quarantined(&d.id))
    }

    fn subtree_delivery_id_quarantined(&self, id: &str) -> bool {
        self.orchestration
            .pauses
            .records
            .values()
            .any(|p| p.quarantined.iter().any(|q| q == id))
    }

    pub(super) fn quarantine_subtree_delivery(&mut self, delivery: &Delivery) {
        let Some(pane) = self.public_pane_id_of_terminal(&delivery.terminal) else {
            return;
        };
        let mut changed = false;
        for pause in self
            .orchestration
            .pauses
            .records
            .values_mut()
            .filter(|p| p.holds_automation() && p.contains(&pane))
        {
            if !pause.quarantined.contains(&delivery.id) {
                pause.quarantined.push(delivery.id.clone());
                pause.deliveries.push(store::StoredDelivery {
                    id: Some(delivery.id.clone()),
                    pane_id: pane.clone(),
                    text: delivery.text.clone(),
                    source: delivery.source.clone(),
                    queued_at_ms: delivery.queued_at_ms,
                    expires_at_ms: delivery.expires_at_ms,
                });
                changed = true;
            }
        }
        if changed {
            let _ = self.save_subtree_pauses();
        }
    }

    pub(super) fn subtree_job_suspended(&self, name: &str) -> bool {
        if self.orchestration.pauses.fault.is_some() {
            return true;
        }
        if self.orchestration.pauses.records.values().any(|p| {
            p.jobs.iter().any(|j| j.name == name)
                && (p.active() || !p.restored_jobs.iter().any(|j| j == name))
        }) {
            return true;
        }
        self.orchestration.jobs.get(name).is_some_and(|job| {
            job.spec
                .target
                .as_ref()
                .is_some_and(|t| self.subtree_terminal_paused(t))
                || job
                    .spec
                    .owner
                    .as_ref()
                    .is_some_and(|t| self.subtree_terminal_paused(t))
        })
    }

    fn pause_pane_name(&self, terminal: &TerminalId) -> String {
        self.public_pane_id_of_terminal(terminal)
            .or_else(|| {
                self.orchestration
                    .last_seen
                    .get(terminal)
                    .map(|s| s.pane_id.clone())
            })
            .unwrap_or_else(|| terminal.to_string())
    }

    fn pause_identity(&self, pane: &str) -> Value {
        let Ok(terminal) = self.terminal_of_target(pane) else {
            return Value::Null;
        };
        let Some((ws, id)) = self.terminal_location(&terminal) else {
            return Value::Null;
        };
        let Some(metadata) = self.pane_metadata(ws, id) else {
            return Value::Null;
        };
        let live = self.agent_info(ws, id);
        json!({"name":live.as_ref().and_then(|a| a.name.clone()),
            "agent":metadata.agent,"session":metadata.agent_session,"cwd":metadata.cwd,
            "live":live.is_some()})
    }

    fn pause_problems(&self, pause: &PauseRecord) -> Vec<String> {
        let mut problems = Vec::new();
        if let Some(error) = &self.orchestration.pauses.fault {
            problems.push(format!("persistence: {error}"));
        }
        for m in &pause.members {
            if self.pause_identity(&m.pane_id) != m.identity {
                problems.push(format!("identity changed or missing: {}", m.pane_id));
            }
        }
        for expected in &pause.expected {
            if !Self::expected_identity_matches(expected, &self.pause_identity(&expected.pane_id)) {
                problems.push(format!(
                    "fleet ownership identity changed or missing: {}",
                    expected.pane_id
                ));
            }
        }
        let current = self.stored_orchestration_state();
        for expected in &pause.links {
            if !current.links.iter().any(|link| link == expected) {
                problems.push(format!(
                    "topology changed: {} -> {}",
                    expected.pane_id, expected.supervisor_pane_id
                ));
            }
        }
        for actual in &current.links {
            if (pause.contains(&actual.pane_id) || pause.contains(&actual.supervisor_pane_id))
                && !pause.links.contains(actual)
            {
                problems.push(format!(
                    "unexpected link: {} -> {}",
                    actual.pane_id, actual.supervisor_pane_id
                ));
            }
        }
        for saved in &pause.jobs {
            if self
                .orchestration
                .jobs
                .get(&saved.name)
                .is_some_and(|job| job.running.is_some())
            {
                problems.push(format!("job still running: {}", saved.name));
            }
        }
        for member in &pause.members {
            if self
                .terminal_of_target(&member.pane_id)
                .ok()
                .is_some_and(|t| self.orchestration.in_flight.contains_key(&t))
            {
                problems.push(format!("input already in flight: {}", member.pane_id));
            }
        }
        problems
    }

    fn pause_receipt(&self, pause: &PauseRecord) -> Value {
        let problems = self.pause_problems(pause);
        let confirmed: Vec<_> = pause
            .members
            .iter()
            .filter(|m| m.checkpoint.is_some() && self.pause_identity(&m.pane_id) == m.identity)
            .map(|m| &m.pane_id)
            .collect();
        let pending: Vec<_> = pause
            .members
            .iter()
            .filter(|m| m.checkpoint.is_none() || self.pause_identity(&m.pane_id) != m.identity)
            .map(|m| &m.pane_id)
            .collect();
        let state = if !pause.active() {
            pause.state.as_str()
        } else if !problems.is_empty() {
            "failed"
        } else if pending.is_empty() {
            "paused"
        } else {
            "pausing"
        };
        let mut receipt = serde_json::to_value(pause).unwrap_or(Value::Null);
        if let Some(members) = receipt["members"].as_array_mut() {
            for member in members {
                let pane = member["pane_id"].as_str().unwrap_or_default();
                let reason = self
                    .terminal_of_target(pane)
                    .ok()
                    .and_then(|t| self.pane_readiness(&t))
                    .and_then(super::deliveries::PaneReadiness::hold)
                    .unwrap_or("awaiting_checkpoint");
                if member["checkpoint"].is_null() {
                    member["held_by"] = json!(reason);
                }
            }
        }
        // Environment values are needed for recovery, not for an operator-facing receipt.
        if let Some(jobs) = receipt["jobs"].as_array_mut() {
            for job in jobs {
                if let Some(job) = job.as_object_mut() {
                    job.remove("env");
                }
            }
        }
        receipt["state"] = json!(state);
        receipt["problems"] = json!(problems);
        receipt["confirmed"] = json!(confirmed);
        receipt["pending"] = json!(pending);
        receipt["automation_fenced"] =
            json!(pause.active() && self.orchestration.pauses.fault.is_none());
        receipt
    }

    /// Both the headless admission path and the in-process dispatcher call this before mutations.
    pub(crate) fn subtree_request_error(&mut self, request: &Request) -> Option<String> {
        self.ensure_orchestration_loaded();
        let active = self.orchestration.pauses.fault.is_some()
            || self
                .orchestration
                .pauses
                .records
                .values()
                .any(PauseRecord::active);
        let method = crate::api::api_method_name(&request.method);
        let held_job = match &request.method {
            crate::api::schema::Method::JobRun(p) => self.subtree_job_suspended(&p.name),
            crate::api::schema::Method::JobAdd(p) => self.subtree_job_suspended(&p.name),
            _ => false,
        };
        if !active && !held_job {
            return None;
        }
        if matches!(
            method,
            "subtree.control"
                | "agent.list"
                | "agent.get"
                | "agent.read"
                | "agent.explain"
                | "agent.deliveries"
                | "job.list"
                | "pane.list"
                | "pane.get"
                | "pane.read"
                | "pane.current"
                | "pane.process_info"
                | "pane.layout"
                | "pane.shell_state"
                | "session.snapshot"
                | "workspace.list"
                | "workspace.get"
                | "worktree.list"
                | "tab.list"
                | "tab.get"
                | "ping"
                | "events.read"
                | "events.subscribe"
                | "events.wait"
                | "pane.report_agent"
                | "pane.report_agent_session"
                | "pane.report_metadata"
                | "pane.wait_for_output"
                | "agent.wait"
                | "notification.show"
        ) {
            return None;
        }
        let opaque = matches!(
            method,
            "server.stop"
                | "server.live_handoff"
                | "command.invoke"
                | "layout.apply"
                | "plugin.action.invoke"
                | "plugin.pane.close"
        );
        let mut protected =
            (active && opaque) || held_job || self.orchestration.pauses.fault.is_some();
        let value = serde_json::to_value(&request.method).ok()?;
        let params = &value["params"];
        if method.starts_with("job.") {
            if let Some(name) = params["name"].as_str() {
                protected |= if method == "job.remove" {
                    self.subtree_job_definition_protected(name)
                } else {
                    self.subtree_job_suspended(name)
                };
            }
        }
        if method == "agent.cancel_delivery" {
            if let Some(id) = params["delivery_id"].as_str() {
                protected |= self.subtree_delivery_id_quarantined(id);
            }
        }
        let close_workspace = match &request.method {
            crate::api::schema::Method::WorkspaceClose(p) => self
                .parse_workspace_id(&p.workspace_id)
                .map(|i| (i, p.close_group)),
            crate::api::schema::Method::WorktreeRemove(p) => {
                self.parse_workspace_id(&p.workspace_id).map(|i| (i, false))
            }
            crate::api::schema::Method::TabClose(p) => self
                .parse_tab_id(&p.tab_id)
                .filter(|(i, _)| self.state.workspaces[*i].tabs.len() == 1)
                .map(|(i, _)| (i, false)),
            crate::api::schema::Method::PaneClose(p) => self
                .parse_pane_id(&p.pane_id)
                .filter(|(i, _)| {
                    let w = &self.state.workspaces[*i];
                    w.tabs.len() == 1 && w.tabs[0].panes.len() == 1
                })
                .map(|(i, _)| (i, false)),
            _ => None,
        };
        if let Some((index, group)) = close_workspace {
            let indices = if group {
                self.state.workspace_group_close_indices(index)
            } else {
                self.state.workspace_close_indices(index)
            };
            protected |= indices.into_iter().any(|i| {
                self.state.workspaces[i].tabs.iter().any(|tab| {
                    tab.panes
                        .values()
                        .any(|p| self.subtree_terminal_paused(&p.attached_terminal_id))
                })
            });
        }
        if matches!(method, "pane.split" | "pane.swap")
            && [
                "pane_id",
                "target_pane_id",
                "source_pane_id",
                "workspace_id",
            ]
            .iter()
            .all(|k| params[*k].is_null())
        {
            protected |= self
                .state
                .active
                .and_then(|i| {
                    let ws = &self.state.workspaces[i];
                    ws.focused_pane_id().and_then(|p| ws.terminal_id(p))
                })
                .is_some_and(|t| self.subtree_terminal_paused(t));
        }
        // Only addressing fields count. Prompt text and job arguments are data, not targets.
        for key in [
            "target",
            "pane_id",
            "supervisor",
            "from",
            "owner_pane_id",
            "workspace_id",
            "tab_id",
            "terminal_id",
            "source_pane_id",
            "target_pane_id",
        ] {
            let Some(target) = params[key].as_str() else {
                continue;
            };
            if let Ok(terminal) = self.terminal_of_target(target) {
                protected |= self.subtree_terminal_paused(&terminal);
            }
            // Pane/tab/workspace handlers also accept legacy addressing aliases.
            if matches!(key, "pane_id" | "source_pane_id" | "target_pane_id")
                && method != "pane.supervise"
            {
                protected |= self
                    .parse_pane_id(target)
                    .and_then(|(w, p)| self.state.workspaces[w].terminal_id(p))
                    .is_some_and(|t| self.subtree_terminal_paused(t));
            }
            if key == "tab_id" {
                protected |= self.parse_tab_id(target).is_some_and(|(w, t)| {
                    self.state.workspaces[w].tabs[t]
                        .panes
                        .values()
                        .any(|p| self.subtree_terminal_paused(&p.attached_terminal_id))
                });
            }
            if key == "workspace_id" {
                protected |= self.parse_workspace_id(target).is_some_and(|w| {
                    self.state.workspaces[w].tabs.iter().any(|t| {
                        t.panes
                            .values()
                            .any(|p| self.subtree_terminal_paused(&p.attached_terminal_id))
                    })
                });
            }
            for pause in self
                .orchestration
                .pauses
                .records
                .values()
                .filter(|p| p.active())
            {
                protected |= pause.members.iter().any(|m| {
                    m.pane_id == target
                        || m.agent["workspace_id"].as_str() == Some(target)
                        || m.agent["tab_id"].as_str() == Some(target)
                        || m.agent["terminal_id"].as_str() == Some(target)
                });
            }
        }
        protected.then(|| {
            encode_error(
                request.id.clone(),
                "subtree_paused",
                "subtree paused; use explicit subtree resume or release after reconciliation",
            )
        })
    }

    fn capture_pause_record(
        &self,
        pause_id: &str,
        root: String,
        descendants: Vec<(TerminalId, usize)>,
    ) -> PauseRecord {
        let terminals: HashSet<_> = descendants.iter().map(|(t, _)| t.clone()).collect();
        let members: Vec<_> = descendants.into_iter().map(|(t,depth)| {
                let pane=self.pause_pane_name(&t);
                let identity=self.pause_identity(&pane);
                let agent=self.terminal_location(&t).and_then(|(ws,p)|self.pane_metadata(ws,p))
                    .and_then(|m|serde_json::to_value(m).ok()).unwrap_or_else(||json!({"terminal_id":t.to_string()}));
                let identity=if identity.is_null() { json!({"name":self.orchestration.last_seen.get(&t).map(|s|&s.name),"unreachable":true}) } else {identity};
                Member {pane_id:pane,identity,agent,depth,requested:false,checkpoint:None}
            }).collect();
        let jobs = self
            .orchestration
            .jobs
            .iter()
            .filter(|j| {
                j.spec.owner.as_ref().is_some_and(|t| terminals.contains(t))
                    || j.spec
                        .target
                        .as_ref()
                        .is_some_and(|t| terminals.contains(t))
            })
            .map(|j| {
                let spec = &j.spec;
                store::StoredJob {
                    name: spec.name.clone(),
                    command: spec.command.clone(),
                    cwd: spec.cwd.clone(),
                    env: spec.env.clone(),
                    every_ms: spec.every.map(|t| t.as_millis() as u64),
                    on_events: spec.on_events.clone(),
                    on_path: spec.on_path.as_ref().map(|p| p.display().to_string()),
                    target_pane_id: spec.target.as_ref().map(|t| self.pause_pane_name(t)),
                    owner_pane_id: spec.owner.as_ref().map(|t| self.pause_pane_name(t)),
                    timeout_ms: spec.timeout.as_millis() as u64,
                    until_ms: spec.until_ms,
                    dedupe: spec.dedupe,
                    last_news: j.last_news.clone(),
                }
            })
            .collect();
        let mut deliveries: Vec<_> = self
            .orchestration
            .in_flight
            .values()
            .map(|f| &f.delivery)
            .chain(self.orchestration.deliveries.iter())
            .filter(|d| terminals.contains(&d.terminal))
            .map(|d| store::StoredDelivery {
                id: Some(d.id.clone()),
                pane_id: self.pause_pane_name(&d.terminal),
                text: d.text.clone(),
                source: d.source.clone(),
                queued_at_ms: d.queued_at_ms,
                expires_at_ms: d.expires_at_ms,
            })
            .collect();
        deliveries.sort_by(|a, b| a.queued_at_ms.cmp(&b.queued_at_ms).then(a.id.cmp(&b.id)));
        let mut links: Vec<_> = self
            .orchestration
            .links
            .iter()
            .filter(|(t, l)| terminals.contains(*t) || terminals.contains(&l.supervisor))
            .map(|(t, l)| store::StoredLink {
                pane_id: self.pause_pane_name(t),
                supervisor_pane_id: self.pause_pane_name(&l.supervisor),
                report: l.report.clone(),
            })
            .collect();
        links.sort_by(|a, b| a.pane_id.cmp(&b.pane_id));
        PauseRecord {
            id: pause_id.into(),
            root,
            state: "pausing".into(),
            created_ms: now_ms(),
            members,
            observed_links: links.clone(),
            expected: Vec::new(),
            links,
            quarantined: deliveries.iter().filter_map(|d| d.id.clone()).collect(),
            deliveries,
            jobs,
            restored_jobs: Vec::new(),
            authorization: None,
            reconciliations: Vec::new(),
        }
    }

    pub(crate) fn handle_subtree_control(
        &mut self,
        id: String,
        params: SubtreeControlParams,
    ) -> String {
        self.ensure_orchestration_loaded();
        if self.orchestration.pauses.unreadable {
            return encode_error(
                id,
                "pause_state_unreadable",
                self.orchestration.pauses.fault.clone().unwrap_or_default(),
            );
        }
        let Some(pause_id) = params.pause_id.as_deref().filter(|s| {
            !s.is_empty()
                && s.len() <= 128
                && s.bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c))
        }) else {
            return encode_error(
                id,
                "invalid_pause_id",
                "provide a stable --id (letters, digits, dash, underscore or dot)",
            );
        };
        if params.action == "pause" && !self.orchestration.pauses.records.contains_key(pause_id) {
            let Some(target) = params.target.as_deref() else {
                return encode_error(id, "target_required", "pause requires a root target");
            };
            let terminal = match self.terminal_of_target(target) {
                Ok(t) => t,
                Err(e) => return super::target_error(id, e),
            };
            let Some(root) = self.public_pane_id_of_terminal(&terminal) else {
                return encode_error(id, "pane_not_found", "root has no pane");
            };
            let descendants = self.pause_descendants(vec![terminal]);
            let record = self.capture_pause_record(pause_id, root, descendants);
            // Install the in-memory fence before any effect, and persist before acknowledging it.
            self.orchestration
                .pauses
                .records
                .insert(pause_id.into(), record);
            self.orchestration.dirty = true;
            self.save_orchestration_now();
        }
        let Some(mut record) = self.orchestration.pauses.records.get(pause_id).cloned() else {
            return encode_error(id, "pause_not_found", "no saved pause with this id");
        };
        match params.action.as_str() {
            "pause" => {
                if !record.active() {
                    return encode_error(id, "pause_already_ended", "use a new pause id");
                }
                if let Some(target) = params.target.as_deref() {
                    if self
                        .terminal_of_target(target)
                        .ok()
                        .and_then(|t| self.public_pane_id_of_terminal(&t))
                        .as_deref()
                        != Some(&record.root)
                    {
                        return encode_error(
                            id,
                            "pause_root_changed",
                            "this id belongs to a different root",
                        );
                    }
                }
                record = match self.extend_pause_ownership(record, &params.expected) {
                    Ok(record) => record,
                    Err(error) => return encode_error(id, "invalid_pause_expectation", error),
                };
                self.orchestration
                    .pauses
                    .records
                    .insert(pause_id.into(), record.clone());
                if let Err(e) = self.save_subtree_pauses() {
                    return encode_error(id, "pause_persist_failed", e);
                }
                self.orchestration.wake();
            }
            "status" => {}
            "reconcile" => {
                let Some(authorization) = params
                    .authorization
                    .filter(|a| !a.trim().is_empty() && a.len() <= 4096)
                else {
                    return encode_error(id,"authorization_required","explicit user authorization is required for identity/topology reconciliation");
                };
                let Some(evidence) = params
                    .evidence
                    .filter(|e| !e.trim().is_empty() && e.len() <= 4096)
                else {
                    return encode_error(
                        id,
                        "checkpoint_required",
                        "reconciliation requires an inspected durable checkpoint",
                    );
                };
                if !record.active() {
                    return encode_error(
                        id,
                        "pause_already_ended",
                        "only an active pause can be reconciled",
                    );
                }
                let pane = params
                    .target
                    .as_deref()
                    .and_then(|t| self.terminal_of_target(t).ok())
                    .and_then(|t| self.public_pane_id_of_terminal(&t))
                    .unwrap_or_default();
                let Some(index) = record.members.iter().position(|m| m.pane_id == pane) else {
                    return encode_error(id, "not_in_subtree", "target is not in this pause");
                };
                if record.members.iter().any(|m| {
                    record.below(&m.pane_id, &pane)
                        && (m.checkpoint.is_none() || self.pause_identity(&m.pane_id) != m.identity)
                }) {
                    return encode_error(
                        id,
                        "descendants_unconfirmed",
                        "reconcile descendants first",
                    );
                }
                if record.jobs.iter().any(|j| {
                    self.orchestration
                        .jobs
                        .get(&j.name)
                        .is_some_and(|j| j.running.is_some())
                }) || record.members.iter().any(|m| {
                    self.terminal_of_target(&m.pane_id)
                        .ok()
                        .is_some_and(|t| self.orchestration.in_flight.contains_key(&t))
                }) {
                    return encode_error(id,"pause_reconcile_required","wait for already-running jobs and terminal writes; do not kill application processes");
                }
                let terminal = match self.terminal_of_target(&pane) {
                    Ok(t) => t,
                    Err(e) => return super::target_error(id, e),
                };
                let expected = record.links.iter().find(|l| l.pane_id == pane).cloned();
                let supervisor = match expected.as_ref() {
                    Some(l) => match self.terminal_of_target(&l.supervisor_pane_id) {
                        Ok(t) => Some(t),
                        Err(e) => return super::target_error(id, e),
                    },
                    None => None,
                };
                if supervisor.as_ref().is_some_and(|s| {
                    super::supervision::would_cycle(&self.orchestration.links, &terminal, s)
                }) {
                    return encode_error(
                        id,
                        "supervision_cycle",
                        "reconciliation would introduce a supervision cycle",
                    );
                }
                let current = self.pause_identity(&pane);
                record.reconciliations.push(json!({"pane_id":pane,"previous":record.members[index].identity,
                    "current":current,"authorization":authorization,"evidence":evidence,"at_ms":now_ms()}));
                for expected in record.expected.iter_mut().filter(|e| e.pane_id == pane) {
                    expected.agent = current["name"].as_str().map(str::to_string);
                    expected.session = serde_json::from_value(current["session"].clone()).ok();
                }
                record.members[index].identity = current;
                record.members[index].checkpoint = Some(evidence);
                record.members[index].requested = true;
                self.orchestration
                    .pauses
                    .records
                    .insert(pause_id.into(), record.clone());
                if let Err(e) = self.save_subtree_pauses() {
                    return encode_error(id, "pause_persist_failed", e);
                }
                match (expected, supervisor) {
                    (Some(link), Some(supervisor)) => {
                        self.orchestration.links.insert(
                            terminal,
                            super::supervision::Supervision {
                                supervisor,
                                report: link.report,
                            },
                        );
                    }
                    _ => {
                        self.orchestration.links.remove(&terminal);
                    }
                }
                if let Some(path) = self.orchestration.store_path.as_ref() {
                    if let Err(e) = store::save(path, &self.stored_orchestration_state()) {
                        self.orchestration.pauses.fault = Some(e.to_string());
                        return encode_error(id, "pause_persist_failed", e.to_string());
                    }
                }
            }
            "checkpoint" | "request" => {
                if !record.active() {
                    return encode_error(id, "pause_already_ended", "pause is no longer active");
                }
                if !self.pause_problems(&record).is_empty() {
                    return encode_error(id, "pause_reconcile_required", "inspect status; topology, identity, persistence or active jobs need reconciliation");
                }
                let Some(target) = params.target.as_deref() else {
                    return encode_error(id, "target_required", "checkpoint requires a target");
                };
                let pane = self
                    .terminal_of_target(target)
                    .ok()
                    .and_then(|t| self.public_pane_id_of_terminal(&t))
                    .unwrap_or_default();
                let Some(index) = record.members.iter().position(|m| m.pane_id == pane) else {
                    return encode_error(id, "not_in_subtree", "target is not in this pause");
                };
                if params.action == "checkpoint" {
                    let Some(evidence) = params
                        .evidence
                        .filter(|e| !e.trim().is_empty() && e.len() <= 4096)
                    else {
                        return encode_error(id, "checkpoint_required", "provide a durable checkpoint reference attesting owned autonomous work and wakes stopped");
                    };
                    if record
                        .members
                        .iter()
                        .any(|m| m.checkpoint.is_none() && record.below(&m.pane_id, &pane))
                    {
                        return encode_error(
                            id,
                            "descendants_unconfirmed",
                            "checkpoint descendants first",
                        );
                    }
                    record.members[index].checkpoint = Some(evidence);
                } else if record.members[index].checkpoint.is_none() {
                    record.members[index].requested = false;
                }
                self.orchestration
                    .pauses
                    .records
                    .insert(pause_id.into(), record.clone());
                if let Err(e) = self.save_subtree_pauses() {
                    return encode_error(id, "pause_persist_failed", e);
                }
                self.orchestration.wake();
            }
            "resume" | "release" => {
                let Some(authorization) = params
                    .authorization
                    .filter(|a| !a.trim().is_empty() && a.len() <= 4096)
                else {
                    return encode_error(
                        id,
                        "authorization_required",
                        "explicit user authorization and reconciliation are required",
                    );
                };
                if params.action == "resume" && record.state == "released" {
                    return encode_error(id,"pause_already_ended","release ended ownership; start a fresh attempt rather than resume this pause");
                }
                if params.action == "release" && !params.restore_jobs.is_empty() {
                    return encode_error(
                        id,
                        "invalid_restore_jobs",
                        "release cannot rearm wakeups",
                    );
                }
                if record.active() || (params.action == "release" && record.state != "released") {
                    if params.action == "resume"
                        && (!self.pause_problems(&record).is_empty()
                            || record.members.iter().any(|m| m.checkpoint.is_none()))
                    {
                        return encode_error(id, "pause_reconcile_required", "all identities, edges, active jobs and checkpoints must reconcile first");
                    }
                    let current = self.stored_orchestration_state();
                    for name in &params.restore_jobs {
                        let saved = record.jobs.iter().find(|j| &j.name == name);
                        let actual = current.jobs.iter().find(|j| &j.name == name);
                        let applicable = match (saved, actual) {
                            (Some(a), Some(b)) => {
                                let mut a = a.clone();
                                let mut b = b.clone();
                                a.last_news = None;
                                b.last_news = None;
                                a == b && a.until_ms.is_none_or(|t| t > now_ms())
                            }
                            _ => false,
                        };
                        if !applicable {
                            return encode_error(
                                id,
                                "job_reconcile_required",
                                format!("saved job changed, expired or missing: {name}"),
                            );
                        }
                    }
                    if params.action == "release" {
                        let coverage = self.pause_receipt(&record);
                        record.reconciliations.push(
                            json!({"release_authorization":authorization,"at_ms":now_ms(),
                            "pending":coverage["pending"],"problems":coverage["problems"]}),
                        );
                    }
                    record.state = if params.action == "resume" {
                        "resumed"
                    } else {
                        "released"
                    }
                    .into();
                    record.authorization = Some(authorization);
                    record.restored_jobs = params.restore_jobs;
                    self.orchestration
                        .pauses
                        .records
                        .insert(pause_id.into(), record.clone());
                    if let Err(e) = self.save_subtree_pauses() {
                        // A failed commit cannot open the in-memory gate.
                        record.state = "pausing".into();
                        self.orchestration
                            .pauses
                            .records
                            .insert(pause_id.into(), record);
                        return encode_error(id, "pause_persist_failed", e);
                    }
                }
                // Stale lines remain archived in the pause record, never replayed. Mail is untouched.
                for delivery in &record.quarantined {
                    self.orchestration.deliveries.cancel(delivery);
                }
                self.orchestration.dirty = true;
                self.save_orchestration_now();
            }
            _ => {
                return encode_error(
                    id,
                    "invalid_pause_action",
                    "use pause, status, request, checkpoint, reconcile, resume or release",
                )
            }
        }
        encode_success(
            id,
            ResponseResult::SubtreeControl {
                pause: self.pause_receipt(&record),
            },
        )
    }

    pub(super) fn request_subtree_checkpoints(&mut self) {
        let mut requests = Vec::new();
        for record in self
            .orchestration
            .pauses
            .records
            .values()
            .filter(|p| p.active())
        {
            if record
                .members
                .iter()
                .all(|m| m.requested || m.checkpoint.is_some())
            {
                continue;
            }
            if self.orchestration.pauses.fault.is_some() {
                continue;
            }
            for member in &record.members {
                let identity = self.pause_identity(&member.pane_id);
                if identity != member.identity
                    || record
                        .expected
                        .iter()
                        .filter(|e| e.pane_id == member.pane_id)
                        .any(|e| !Self::expected_identity_matches(e, &identity))
                {
                    continue;
                }

                if member.requested
                    || member.checkpoint.is_some()
                    || record.members.iter().any(|m| {
                        m.checkpoint.is_none() && record.below(&m.pane_id, &member.pane_id)
                    })
                {
                    continue;
                }
                let Ok(terminal) = self.terminal_of_target(&member.pane_id) else {
                    continue;
                };
                if !self
                    .pane_readiness(&terminal)
                    .is_some_and(super::deliveries::PaneReadiness::safe)
                    || self.orchestration.in_flight.contains_key(&terminal)
                {
                    continue;
                }
                requests.push((record.id.clone(), member.pane_id.clone(), terminal));
            }
        }
        for (id, pane, terminal) in requests {
            let Some((ws, p)) = self.terminal_location(&terminal) else {
                continue;
            };
            let Some(agent) = self
                .state
                .terminals
                .get(&terminal)
                .and_then(|s| s.effective_known_agent())
            else {
                continue;
            };
            if let Some(member) = self
                .orchestration
                .pauses
                .records
                .get_mut(&id)
                .and_then(|r| r.members.iter_mut().find(|m| m.pane_id == pane))
            {
                member.requested = true;
            }
            // Record before typing: after a crash an operator may retry a request, never ordinary work.
            if self.save_subtree_pauses().is_err() {
                return;
            }
            let text = format!("PAUSE {id}: Stop autonomous work and owned external wakes without teardown. Preserve hierarchy, mail and changes. Write a durable checkpoint, then attest it with: herdr subtree checkpoint --id {id} --target {pane} --evidence CHECKPOINT_PATH. Do not resume without explicit user authorization. This is a pause, not worker_done.");
            if self
                .submit_text_to_agent(ws, p, agent, &text, None)
                .is_err()
            {
                if let Some(member) = self
                    .orchestration
                    .pauses
                    .records
                    .get_mut(&id)
                    .and_then(|r| r.members.iter_mut().find(|m| m.pane_id == pane))
                {
                    member.requested = false;
                }
                let _ = self.save_subtree_pauses();
            }
        }
    }
}

impl App {
    fn pause_descendants(&self, roots: Vec<TerminalId>) -> Vec<(TerminalId, usize)> {
        let mut descendants = Vec::new();
        for root in roots {
            if !descendants.iter().any(|(t, _)| t == &root) {
                descendants.push((root, 0));
            }
        }
        let mut index = 0;
        while let Some((parent, depth)) = descendants.get(index).cloned() {
            index += 1;
            let mut children: Vec<_> = self
                .orchestration
                .links
                .iter()
                .filter(|(_, l)| l.supervisor == parent)
                .map(|(t, _)| t.clone())
                .collect();
            children.sort_by_key(|t| self.pause_pane_name(t));
            for child in children {
                if !descendants.iter().any(|(t, _)| t == &child) {
                    descendants.push((child, depth + 1));
                }
            }
        }
        descendants
    }

    fn pause_expected_target(&self, target: &str) -> Option<TerminalId> {
        self.terminal_of_target(target).ok().or_else(|| {
            self.parse_pane_id(target)
                .and_then(|(w, p)| self.state.workspaces[w].terminal_id(p).cloned())
        })
    }

    fn expected_identity_matches(expected: &ExpectedSubtreeMember, identity: &Value) -> bool {
        let session_key =
            |s: &Value| json!({"agent":s["agent"],"kind":s["kind"],"value":s["value"]});
        expected
            .agent
            .as_deref()
            .is_none_or(|name| identity["name"].as_str() == Some(name))
            && expected.session.as_ref().is_none_or(|session| {
                json!({"agent":session.agent,"kind":session.kind,"value":session.value})
                    == session_key(&identity["session"])
            })
    }

    fn extend_pause_ownership(
        &self,
        mut record: PauseRecord,
        expected: &[ExpectedSubtreeMember],
    ) -> Result<PauseRecord, String> {
        let mut changed_scope = HashSet::new();
        for original in expected {
            if original.pane_id.trim().is_empty() || original.supervisor_pane_id.trim().is_empty() {
                return Err("expected pane and supervisor are required".into());
            }
            let mut owned = original.clone();
            let terminal = self.pause_expected_target(&owned.pane_id);
            if let Some(t) = terminal.as_ref() {
                owned.pane_id = self.pause_pane_name(t);
            }
            if let Some(t) = self.pause_expected_target(&owned.supervisor_pane_id) {
                owned.supervisor_pane_id = self.pause_pane_name(&t);
            }
            if owned.pane_id == owned.supervisor_pane_id {
                return Err("an expected member cannot supervise itself".into());
            }
            // Repeated pause must not overwrite an explicitly reconciled identity or narrow scope.
            if record.expected.iter().any(|e| e.pane_id == owned.pane_id) {
                continue;
            }
            if !record.contains(&owned.pane_id)
                || record
                    .links
                    .iter()
                    .find(|l| l.pane_id == owned.pane_id)
                    .is_none_or(|l| l.supervisor_pane_id != owned.supervisor_pane_id)
            {
                changed_scope.insert(owned.pane_id.clone());
            }
            if let Some(terminal) = terminal.filter(|_| !record.contains(&owned.pane_id)) {
                let extra = self.capture_pause_record(
                    &record.id,
                    owned.pane_id.clone(),
                    self.pause_descendants(vec![terminal]),
                );
                for member in extra.members {
                    if !record.contains(&member.pane_id) {
                        record.members.push(member);
                    }
                }
                for job in extra.jobs {
                    if !record.jobs.iter().any(|j| j.name == job.name) {
                        record.jobs.push(job);
                    }
                }
                for delivery in extra.deliveries {
                    if !record.deliveries.iter().any(|d| d.id == delivery.id) {
                        record.deliveries.push(delivery);
                    }
                }
                for id in extra.quarantined {
                    if !record.quarantined.contains(&id) {
                        record.quarantined.push(id);
                    }
                }
                for link in extra.links {
                    if !record.observed_links.contains(&link) {
                        record.observed_links.push(link.clone());
                    }
                    if !record.links.iter().any(|l| l.pane_id == link.pane_id) {
                        record.links.push(link);
                    }
                }
            } else if !record.contains(&owned.pane_id) {
                record.members.push(Member {
                    pane_id: owned.pane_id.clone(),
                    identity: json!({"name":owned.agent,"unreachable":true}),
                    agent: Value::Null,
                    depth: 1,
                    requested: false,
                    checkpoint: None,
                });
            }
            let report = record
                .links
                .iter()
                .find(|l| l.pane_id == owned.pane_id)
                .map(|l| l.report.clone())
                .unwrap_or_default();
            record.links.retain(|l| l.pane_id != owned.pane_id);
            record.links.push(store::StoredLink {
                pane_id: owned.pane_id.clone(),
                supervisor_pane_id: owned.supervisor_pane_id.clone(),
                report,
            });
            record.expected.push(owned);
        }
        for owned in &record.expected {
            if !record.contains(&owned.supervisor_pane_id) {
                return Err(format!(
                    "expected supervisor is outside the pause scope: {}",
                    owned.supervisor_pane_id
                ));
            }
        }
        for member in &record.members {
            let mut seen = HashSet::new();
            let mut pane = member.pane_id.as_str();
            while let Some(link) = record.links.iter().find(|l| l.pane_id == pane) {
                if !seen.insert(pane) {
                    return Err("expected topology contains a cycle".into());
                }
                pane = &link.supervisor_pane_id;
            }
        }
        let invalidate: HashSet<_> = record
            .members
            .iter()
            .filter(|m| {
                changed_scope
                    .iter()
                    .any(|child| child == &m.pane_id || record.below(child, &m.pane_id))
            })
            .map(|m| m.pane_id.clone())
            .collect();
        for member in &mut record.members {
            if invalidate.contains(&member.pane_id) {
                member.checkpoint = None;
                member.requested = false;
            }
            let mut pane = member.pane_id.as_str();
            for depth in 0..=record.links.len() {
                if pane == record.root {
                    member.depth = depth;
                    break;
                }
                let Some(link) = record.links.iter().find(|l| l.pane_id == pane) else {
                    break;
                };
                pane = &link.supervisor_pane_id;
            }
        }
        record.links.sort_by(|a, b| a.pane_id.cmp(&b.pane_id));
        Ok(record)
    }
}
